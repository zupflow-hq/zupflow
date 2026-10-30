//! Table tests of the reducer: every continuation, every invalid input,
//! deterministic plans. `commit` below is a model of what a store does with
//! each operation, enough to walk whole runs through `advance` without a
//! database; the real store and its guards are held to the conformance
//! suite.

use serde_json::json;
use uuid::Uuid;

use super::*;
use crate::ids::{AgentId, ModelConnectionId, ModelId, SessionId};
use crate::model_store::{RouteState, RunModel};
use crate::models::TokenUsage;
use crate::runs::names::{Candidate, build_offer};
use crate::runs::state::{CallAttempt, CommittedTurn, ModelAttempt, RequestId, RunControl};
use crate::runs::transition::ModelAttemptRef;
use crate::runs::transition::TransitionId;

const NOW: i64 = 1_800_000_000_000;
const WORK_CREATE: &str = "taskboard_k7m2qd_create_task";
const WORK_LIST: &str = "taskboard_k7m2qd_list_tasks";
const PERSONAL_CREATE: &str = "taskboard_p3x7ta_create_task";

fn at(offset_ms: i64) -> Timestamp {
    Timestamp::from_millis(NOW + offset_ms)
}

fn organization() -> OrganizationId {
    OrganizationId::from_uuid(Uuid::from_u128(1))
}

fn workspace() -> WorkspaceId {
    WorkspaceId::from_uuid(Uuid::from_u128(2))
}

fn run_id() -> RunId {
    RunId::from_uuid(Uuid::from_u128(3))
}

fn work() -> InstallationId {
    InstallationId::from_uuid(Uuid::from_u128(10))
}

fn personal() -> InstallationId {
    InstallationId::from_uuid(Uuid::from_u128(11))
}

fn call_id(n: u128) -> ToolCallId {
    ToolCallId::from_uuid(Uuid::from_u128(1000 + n))
}

fn claimant(n: u128) -> ClaimantId {
    ClaimantId::from_uuid(Uuid::from_u128(9000 + n))
}

fn caller(n: u128) -> RequestId {
    RequestId::from_uuid(Uuid::from_u128(7000 + n))
}

fn approval(call_id: ToolCallId, decision: ApprovalDecision, reason: Option<&str>) -> Approval<'_> {
    Approval {
        call_id,
        decision,
        api_key_id: api_key(),
        api_key_name: "Backend",
        reason,
    }
}

/// Who cancels, as the timeline and the audit log name them.
fn canceller() -> AuditActor {
    AuditActor::ApiKey {
        id: api_key(),
        name: "Backend".to_owned(),
    }
}

/// An API key's decision, by caller 1, in the run's own scope.
fn decide(
    snapshot: &Snapshot,
    call_id: ToolCallId,
    decision: ApprovalDecision,
    reason: Option<&str>,
) -> Result<TransitionRequest, ControlError> {
    resolve_approval(
        organization(),
        workspace(),
        snapshot,
        caller(1),
        approval(call_id, decision, reason),
    )
}

fn invocation(n: u128) -> InvocationId {
    InvocationId::from_uuid(Uuid::from_u128(5000 + n))
}

fn api_key() -> ApiKeyId {
    ApiKeyId::from_uuid(Uuid::from_u128(77))
}

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

/// Two Taskboard installations: Work with two tools, Personal with one.
fn offer() -> Offer {
    build_offer([
        candidate(work(), "k7m2qd", "Work", "create_task"),
        candidate(work(), "k7m2qd", "Work", "list_tasks"),
        candidate(personal(), "p3x7ta", "Personal", "create_task"),
    ])
    .offer
}

fn arguments() -> Map<String, Value> {
    let mut arguments = Map::new();
    arguments.insert("title".to_owned(), json!("Q3 notes"));
    arguments
}

/// A run as `admit` leaves it.
fn fresh() -> Snapshot {
    Snapshot {
        organization_id: organization(),
        workspace_id: workspace(),
        run_id: run_id(),
        control: RunControl {
            status: RunStatus::Queued,
            continuation: Continuation::NeedModel { turn: 0 },
            revision: 1,
            owner_epoch: 0,
            owner_claimant: None,
            lease_until: None,
            cancel_requested_at: None,
            execution_version: EXECUTION_VERSION,
        },
        model: "model-a".to_owned(),
        model_id: Some(model_id()),
        connection_id: Some(connection_id()),
        instructions: "Be brief.".to_owned(),
        input: "Make a task.".to_owned(),
        memory: MemoryMode::Agent,
        turns: Vec::new(),
        calls: Vec::new(),
        model_attempts: Vec::new(),
        session: None,
        silent_reason: None,
    }
}

/// A running run held by epoch 1, its lease live for a minute.
fn running(continuation: Continuation) -> Snapshot {
    let mut snapshot = fresh();
    snapshot.control.status = RunStatus::Running;
    snapshot.control.continuation = continuation;
    snapshot.control.revision = 5;
    snapshot.control.owner_epoch = 1;
    snapshot.control.owner_claimant = Some(claimant(1));
    snapshot.control.lease_until = Some(at(60_000));
    snapshot
}

/// Turn 0 prepared as attempt 1, awaiting its answer.
fn awaiting_model() -> Snapshot {
    let mut snapshot = running(Continuation::AwaitModel { turn: 0, attempt: 1 });
    let digest = model_request(&snapshot, 0, 1, None, (&offer(), None), None)
        .expect("a request")
        .digest();
    snapshot.model_attempts.push(ModelAttempt {
        turn: 0,
        attempt: 1,
        state: ModelAttemptState::Prepared,
        request_digest: digest,
        offer: offer(),
        memory: None,
        timed_out: false,
    });
    snapshot
}

/// A call of turn 0 to the offered tool `name`, in `state`.
fn record(n: u128, position: u32, name: &str, state: CallState) -> CallRecord {
    let offered = offer();
    let tool = offered.find(name).expect("offered");
    CallRecord {
        id: call_id(n),
        turn: 0,
        position,
        model_call_id: format!("m{n}"),
        model_tool_name: name.to_owned(),
        target: bound(tool),
        arguments: arguments(),
        state,
        decision: None,
        call_digest: Some(call_digest(
            tool.installation_id.expect("an app tool"),
            &tool.app_key,
            &tool.tool,
            &tool.definition_sha256,
            &arguments(),
        )),
        approval: None,
        outcome: state.is_settled().then(|| CallOutcome {
            content: "done".to_owned(),
            is_error: false,
        }),
        failure: None,
        attempts: Vec::new(),
    }
}

/// Turn 0 committed with `calls`, the run at `continuation`.
fn with_calls(continuation: Continuation, calls: Vec<CallRecord>) -> Snapshot {
    let mut snapshot = running(continuation);
    snapshot.turns.push(CommittedTurn { turn: 0, text: None });
    let digest = model_request(&fresh(), 0, 1, None, (&offer(), None), None)
        .expect("a request")
        .digest();
    snapshot.model_attempts.push(ModelAttempt {
        turn: 0,
        attempt: 1,
        state: ModelAttemptState::Answered,
        request_digest: digest,
        offer: offer(),
        memory: None,
        timed_out: false,
    });
    snapshot.calls = calls;
    snapshot
}

/// Call 1 being sent as attempt 1, its send lease live for `lease_ms`.
fn sending(lease_ms: i64) -> Snapshot {
    let mut call = record(1, 0, WORK_LIST, CallState::Started);
    call.attempts.push(CallAttempt {
        attempt: 1,
        dispatch_epoch: 1,
        invocation_id: invocation(1),
        send_lease_until: at(lease_ms),
        outcome_recorded: false,
    });
    let later = record(2, 1, WORK_CREATE, CallState::Allowed);
    with_calls(
        Continuation::AwaitCall {
            call_id: call_id(1),
            attempt: 1,
        },
        vec![call, later],
    )
}

fn model_id() -> ModelId {
    ModelId::from_uuid(Uuid::from_u128(30))
}

/// Where the session's runs begin: a thread in a channel connection.
fn origin() -> crate::runs::state::ChannelOrigin {
    crate::runs::state::ChannelOrigin {
        connection_id: crate::ids::ChannelConnectionId::from_uuid(Uuid::from_u128(0xc0)),
        conversation_id: crate::ids::ChannelConversationId::from_uuid(Uuid::from_u128(0xc1)),
        thread_key: "1700000000.000100".to_owned(),
    }
}

fn connection_id() -> ModelConnectionId {
    ModelConnectionId::from_uuid(Uuid::from_u128(31))
}

fn facts() -> Facts {
    Facts {
        claimant: Some(claimant(1)),
        offer: Some(offer()),
        model_route: Some(RouteState::Usable { key_version: 3 }),
        ..Facts::at(at(0))
    }
}

fn claimed() -> Facts {
    Facts {
        claim: Some(1),
        ..facts()
    }
}

fn decision(verdict: CallVerdict) -> CallDecision {
    CallDecision {
        verdict,
        policy: "configured".to_owned(),
        mode: Some("manual".to_owned()),
        rule: "allowed".to_owned(),
    }
}

fn allow() -> CallDecision {
    decision(CallVerdict::Allow)
}

fn approve() -> CallDecision {
    decision(CallVerdict::RequireApproval {
        reason: "Needs a person.".to_owned(),
    })
}

fn deny(reason: &str) -> CallDecision {
    decision(CallVerdict::Deny {
        reason: reason.to_owned(),
    })
}

fn check(call: &CallRecord, decision: CallDecision) -> SendCheck {
    SendCheck {
        call_id: call.id,
        installation: InstallationCheck::Usable,
        definition_sha256: call.target.as_ref().map(|target| target.definition_sha256),
        decision,
    }
}

fn completion(reply: Reply) -> Completion {
    Completion {
        reply,
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_input_tokens: 4,
            cache_write_input_tokens: 2,
        },
    }
}

fn answered(turn: u32, attempt: u32, reply: Reply) -> ModelAnswer {
    ModelAnswer {
        turn,
        attempt,
        result: ModelResult::Answered(completion(reply)),
    }
}

fn asks(calls: &[(&str, &str)]) -> Reply {
    Reply::ToolCalls {
        text: Some("On it.".to_owned()),
        calls: calls
            .iter()
            .map(|(id, name)| ModelToolCall {
                id: (*id).to_owned(),
                name: (*name).to_owned(),
                arguments: arguments(),
            })
            .collect(),
    }
}

fn planned(snapshot: &Snapshot, facts: &Facts) -> Plan {
    let first = advance(snapshot, facts).expect("a plan");
    // Deterministic: the same snapshot and facts give the same plan.
    assert_eq!(advance(snapshot, facts).expect("a plan"), first);
    first
}

fn operation(plan: &Plan) -> &Operation {
    plan.request().expect("a transition").operation()
}

fn guards(plan: &Plan) -> Guards {
    *plan.request().expect("a transition").guards()
}

fn keeps(epoch: u64) -> Guards {
    Guards {
        owner: Some(OwnerGuard {
            epoch,
            claimant: claimant(1),
            renew_lease_ms: Some(Limits::default().lease_ms),
        }),
        not_cancelled: true,
        ..Guards::default()
    }
}

fn releases(epoch: u64) -> Guards {
    Guards {
        owner: Some(OwnerGuard {
            epoch,
            claimant: claimant(1),
            renew_lease_ms: None,
        }),
        not_cancelled: true,
        ..Guards::default()
    }
}

fn failure_code(plan: &Plan) -> &str {
    match operation(plan) {
        Operation::Finish(Finish {
            ending: RunEnding::Failed { error },
            ..
        }) => &error.code,
        other => panic!("not a failure: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Before any step

#[test]
fn refuses_runs_it_cannot_advance() {
    for version in [0, EXECUTION_VERSION + 1] {
        let mut snapshot = fresh();
        snapshot.control.execution_version = version;
        assert_eq!(
            advance(&snapshot, &facts()),
            Err(AdvanceError::UnsupportedVersion(version))
        );
        assert_eq!(
            decide(&snapshot, call_id(1), ApprovalDecision::Approved, None),
            Err(ControlError::UnsupportedVersion(version))
        );
    }
    let mut never = fresh();
    never.control.revision = 0;
    assert!(matches!(
        advance(&never, &facts()),
        Err(AdvanceError::InvalidSnapshot(_))
    ));
}

#[test]
fn refuses_a_status_that_disagrees_with_its_continuation() {
    let call = call_id(1);
    let cases = [
        (RunStatus::Queued, Continuation::NeedModel { turn: 1 }),
        (RunStatus::Queued, Continuation::NeedCall { turn: 0, position: 0 }),
        (RunStatus::Queued, Continuation::Done),
        (RunStatus::Running, Continuation::Done),
        (RunStatus::Running, Continuation::AwaitApproval { call_id: call }),
        (RunStatus::WaitingForApproval, Continuation::NeedModel { turn: 0 }),
        (
            RunStatus::WaitingForApproval,
            Continuation::AwaitCall {
                call_id: call,
                attempt: 1,
            },
        ),
        (RunStatus::Completed, Continuation::NeedModel { turn: 0 }),
        (RunStatus::Failed, Continuation::AwaitModel { turn: 0, attempt: 1 }),
        (RunStatus::Cancelled, Continuation::AwaitApproval { call_id: call }),
    ];
    for (status, continuation) in cases {
        let mut snapshot = fresh();
        snapshot.control.status = status;
        snapshot.control.continuation = continuation;
        assert!(
            matches!(advance(&snapshot, &claimed()), Err(AdvanceError::InvalidSnapshot(_))),
            "{status:?} at {continuation:?}"
        );
    }
    // A cancel asked for on a run that isn't cancelled.
    let mut snapshot = running(Continuation::NeedModel { turn: 0 });
    snapshot.control.cancel_requested_at = Some(at(0));
    assert!(matches!(
        advance(&snapshot, &claimed()),
        Err(AdvanceError::InvalidSnapshot(_))
    ));
    // Turns out of order.
    let mut snapshot = running(Continuation::NeedModel { turn: 1 });
    snapshot.turns.push(CommittedTurn { turn: 3, text: None });
    assert!(matches!(
        advance(&snapshot, &claimed()),
        Err(AdvanceError::InvalidSnapshot(_))
    ));
}

#[test]
fn a_finished_run_has_nothing_to_do() {
    for status in [RunStatus::Completed, RunStatus::Failed, RunStatus::Cancelled] {
        let mut snapshot = running(Continuation::Done);
        snapshot.control.status = status;
        snapshot.control.lease_until = None;
        if status == RunStatus::Cancelled {
            snapshot.control.cancel_requested_at = Some(at(-1));
        }
        for facts in [facts(), claimed()] {
            assert_eq!(planned(&snapshot, &facts), Plan::Idle(Idle::Finished), "{status:?}");
        }
    }
}

#[test]
fn a_run_waiting_for_approval_holds_no_claim() {
    let mut snapshot = with_calls(
        Continuation::AwaitApproval { call_id: call_id(1) },
        vec![record(1, 0, WORK_CREATE, CallState::AwaitingApproval)],
    );
    snapshot.control.status = RunStatus::WaitingForApproval;
    snapshot.control.lease_until = None;
    for facts in [facts(), claimed()] {
        assert_eq!(planned(&snapshot, &facts), Plan::Idle(Idle::AwaitingApproval));
    }
}

// ---------------------------------------------------------------------------
// Claims

#[test]
fn the_first_claim_starts_the_run() {
    let snapshot = fresh();
    let plan = planned(&snapshot, &facts());
    assert_eq!(
        operation(&plan),
        &Operation::Claim(Claim {
            epoch: 1,
            claimant: claimant(1),
            lease_ms: Limits::default().lease_ms,
            starts: true,
        })
    );
    assert_eq!(
        guards(&plan),
        Guards {
            not_cancelled: true,
            no_live_claim: true,
            ..Guards::default()
        }
    );
    assert_eq!(plan.io(), &Io::Nothing);
    let request = plan.request().expect("a transition");
    assert_eq!(request.from_revision(), 1);
    assert_eq!(
        request.id(),
        TransitionId::derive(organization(), workspace(), run_id(), 1)
    );
}

#[test]
fn a_lapsed_claim_is_taken_with_a_higher_epoch() {
    let mut snapshot = running(Continuation::NeedModel { turn: 0 });
    snapshot.control.owner_epoch = 3;
    snapshot.control.lease_until = Some(at(-1));
    assert_eq!(
        operation(&planned(&snapshot, &facts())),
        &Operation::Claim(Claim {
            epoch: 4,
            claimant: claimant(1),
            lease_ms: Limits::default().lease_ms,
            starts: false,
        })
    );
    // Exactly at its end, a lease no longer holds.
    snapshot.control.lease_until = Some(at(0));
    assert!(matches!(operation(&planned(&snapshot, &facts())), Operation::Claim(_)));
}

#[test]
fn a_live_lease_or_send_claim_keeps_others_out() {
    let snapshot = running(Continuation::NeedModel { turn: 0 });
    assert_eq!(
        planned(&snapshot, &facts()),
        Plan::Idle(Idle::Busy { until: at(60_000) })
    );
    // The run's lease lapsed, the call's send claim hasn't: wait for it.
    let mut snapshot = sending(30_000);
    snapshot.control.lease_until = Some(at(-1));
    assert_eq!(
        planned(&snapshot, &facts()),
        Plan::Idle(Idle::Busy { until: at(30_000) })
    );
    // Both lapsed: claim, then settle.
    let mut snapshot = sending(-1);
    snapshot.control.lease_until = None;
    assert!(matches!(operation(&planned(&snapshot, &facts())), Operation::Claim(_)));
}

#[test]
fn a_worker_that_lost_its_claim_writes_nothing() {
    let snapshot = running(Continuation::NeedModel { turn: 0 });
    let stale = Facts {
        claim: Some(0),
        ..facts()
    };
    assert_eq!(planned(&snapshot, &stale), Plan::Idle(Idle::Lost));
    let mut lapsed = snapshot.clone();
    lapsed.control.lease_until = Some(at(-1));
    assert_eq!(planned(&lapsed, &claimed()), Plan::Idle(Idle::Lost));
    lapsed.control.lease_until = None;
    assert_eq!(planned(&lapsed, &claimed()), Plan::Idle(Idle::Lost));
}

// ---------------------------------------------------------------------------
// NeedModel

#[test]
fn need_model_prepares_an_attempt_with_the_offer_frozen() {
    let snapshot = running(Continuation::NeedModel { turn: 0 });
    let plan = planned(&snapshot, &claimed());
    let Io::CallModel(request) = plan.io() else {
        panic!("calls the model");
    };
    assert_eq!(
        operation(&plan),
        &Operation::PrepareModel(PrepareModel {
            turn: 0,
            attempt: 1,
            abandons: None,
            request_digest: request.digest(),
            offer: offer(),
            memory: None,
            key_version: Some(3),
            steer: None,
        })
    );
    assert_eq!(guards(&plan), keeps(1));
    assert_eq!((request.turn, request.attempt), (0, 1));
    assert_eq!(
        request.key_version,
        Some(3),
        "the key is opened at the version recorded"
    );
    assert_eq!(request.model, "model-a");
    assert_eq!(request.instructions, "Be brief.");
    assert_eq!(
        request.messages,
        vec![Message::User {
            text: "Make a task.".to_owned()
        }]
    );
    let names: Vec<&str> = request.tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(names, [WORK_CREATE, WORK_LIST, PERSONAL_CREATE]);
    assert!(
        request.tools[2]
            .description
            .starts_with("Taskboard, installation \"Personal\" (p3x7ta).")
    );
}

#[test]
fn calls_the_model_only_while_its_route_is_usable() {
    let snapshot = running(Continuation::NeedModel { turn: 0 });
    let gone = Facts {
        model_route: Some(RouteState::Removed),
        ..claimed()
    };
    let plan = planned(&snapshot, &gone);
    assert_eq!(failure_code(&plan), MODEL_UNAVAILABLE);
    assert!(matches!(plan.io(), Io::Nothing));
    let refused = Facts {
        model_route: Some(RouteState::KeyInvalid {
            connection_label: "Anthropic".to_owned(),
        }),
        ..claimed()
    };
    let plan = planned(&snapshot, &refused);
    assert_eq!(failure_code(&plan), MODEL_KEY_INVALID);
    let Operation::Finish(Finish {
        ending: RunEnding::Failed { error },
        ..
    }) = operation(&plan)
    else {
        panic!("fails");
    };
    assert_eq!(
        error.message,
        "The model provider refused Anthropic's key. Replace it under Model providers."
    );
    // Without the route, nothing is planned.
    let unread = Facts {
        model_route: None,
        ..claimed()
    };
    assert_eq!(
        advance(&snapshot, &unread),
        Err(AdvanceError::MissingFact("model_route"))
    );
    // A run admitted before organizations had models has none to call: it
    // fails rather than sending a deployment's model ID to a provider.
    let mut legacy = snapshot.clone();
    legacy.model_id = None;
    legacy.connection_id = None;
    let plan = planned(&legacy, &unread);
    assert_eq!(failure_code(&plan), MODEL_UNAVAILABLE);
    assert!(matches!(plan.io(), Io::Nothing));
    let Operation::Finish(Finish {
        ending: RunEnding::Failed { error },
        ..
    }) = operation(&plan)
    else {
        panic!("fails");
    };
    assert!(
        error.message.contains("admitted before organizations had models"),
        "{}",
        error.message
    );
}

#[test]
fn fails_an_awaited_model_call_whose_route_went() {
    // The worker that asked the model stopped; the next one finds the model
    // gone and fails the run, abandoning the attempt, instead of asking again.
    let snapshot = awaiting_model();
    let gone = Facts {
        model_route: Some(RouteState::Removed),
        ..claimed()
    };
    let plan = planned(&snapshot, &gone);
    assert_eq!(failure_code(&plan), MODEL_UNAVAILABLE);
    assert!(matches!(plan.io(), Io::Nothing));
    let Operation::Finish(Finish {
        ending: RunEnding::Failed { error },
        abandons,
        ..
    }) = operation(&plan)
    else {
        panic!("fails");
    };
    assert_eq!(
        error.message,
        "The run's model was removed while the run was under way."
    );
    assert_eq!(*abandons, Some(ModelAttemptRef { turn: 0, attempt: 1 }));
    let refused = Facts {
        model_route: Some(RouteState::KeyInvalid {
            connection_label: "OpenAI".to_owned(),
        }),
        ..claimed()
    };
    assert_eq!(failure_code(&planned(&snapshot, &refused)), MODEL_KEY_INVALID);
}

#[test]
fn need_model_carries_every_answer_back() {
    let mut first = record(1, 0, WORK_LIST, CallState::Completed);
    first.outcome = Some(CallOutcome {
        content: "{\"tasks\":[]}".to_owned(),
        is_error: false,
    });
    let mut second = record(2, 1, WORK_CREATE, CallState::Denied);
    second.outcome = Some(CallOutcome {
        content: DENIED.to_owned(),
        is_error: true,
    });
    let mut snapshot = with_calls(Continuation::NeedModel { turn: 1 }, vec![second, first]);
    snapshot.turns[0].text = Some("Looking.".to_owned());
    let plan = planned(&snapshot, &claimed());
    let Io::CallModel(request) = plan.io() else {
        panic!("calls the model");
    };
    assert_eq!(request.messages.len(), 4);
    let Message::Assistant { text, tool_calls } = &request.messages[1] else {
        panic!("the model's turn");
    };
    assert_eq!(text.as_deref(), Some("Looking."));
    let ids: Vec<&str> = tool_calls.iter().map(|call| call.id.as_str()).collect();
    assert_eq!(ids, ["m1", "m2"], "by position");
    assert_eq!(
        request.messages[3],
        Message::ToolResult {
            call_id: "m2".to_owned(),
            content: DENIED.to_owned(),
            is_error: true,
        }
    );
    assert!(matches!(
        operation(&plan),
        Operation::PrepareModel(PrepareModel {
            turn: 1,
            attempt: 1,
            ..
        })
    ));
}

#[test]
fn need_model_stops_at_the_turn_limit_and_yields_past_the_step_budget() {
    let mut snapshot = running(Continuation::NeedModel { turn: 2 });
    snapshot.turns = vec![
        CommittedTurn { turn: 0, text: None },
        CommittedTurn { turn: 1, text: None },
    ];
    let limited = Facts {
        limits: Limits {
            max_turns: 2,
            ..Limits::default()
        },
        ..claimed()
    };
    let plan = planned(&snapshot, &limited);
    assert_eq!(failure_code(&plan), "too_many_turns");
    assert_eq!(guards(&plan), releases(1));

    let snapshot = running(Continuation::NeedModel { turn: 0 });
    let spent = Facts {
        steps: Limits::default().max_steps_per_claim,
        ..claimed()
    };
    let plan = planned(&snapshot, &spent);
    assert_eq!(
        operation(&plan),
        &Operation::Yield(Yield {
            why: YieldReason::StepBudget,
        })
    );
    assert_eq!(guards(&plan), releases(1));
}

#[test]
fn need_model_refuses_what_does_not_fit() {
    let snapshot = running(Continuation::NeedModel { turn: 0 });
    let no_offer = Facts {
        offer: None,
        ..claimed()
    };
    assert_eq!(advance(&snapshot, &no_offer), Err(AdvanceError::MissingFact("offer")));

    let skipped = running(Continuation::NeedModel { turn: 1 });
    assert!(matches!(
        advance(&skipped, &claimed()),
        Err(AdvanceError::InvalidSnapshot(_))
    ));

    let open = with_calls(
        Continuation::NeedModel { turn: 1 },
        vec![record(1, 0, WORK_LIST, CallState::Allowed)],
    );
    assert!(matches!(
        advance(&open, &claimed()),
        Err(AdvanceError::InvalidSnapshot(_))
    ));

    let mut no_outcome = with_calls(
        Continuation::NeedModel { turn: 1 },
        vec![record(1, 0, WORK_LIST, CallState::Completed)],
    );
    no_outcome.calls[0].outcome = None;
    assert!(matches!(
        advance(&no_outcome, &claimed()),
        Err(AdvanceError::InvalidSnapshot(_))
    ));

    let mut prepared = awaiting_model();
    prepared.control.continuation = Continuation::NeedModel { turn: 0 };
    assert!(matches!(
        advance(&prepared, &claimed()),
        Err(AdvanceError::InvalidSnapshot(_))
    ));
}

// ---------------------------------------------------------------------------
// AwaitModel

#[test]
fn await_model_without_an_answer_abandons_the_attempt_and_asks_again() {
    let snapshot = awaiting_model();
    let plan = planned(&snapshot, &claimed());
    let Operation::PrepareModel(prepare) = operation(&plan) else {
        panic!("prepares");
    };
    assert_eq!((prepare.turn, prepare.attempt, prepare.abandons), (0, 2, Some(1)));
    assert!(matches!(plan.io(), Io::CallModel(ModelRequest { attempt: 2, .. })));
}

#[test]
fn a_final_answer_completes_the_run_with_its_turn() {
    let facts = Facts {
        model: Some(answered(0, 1, Reply::Text("Done.".to_owned()))),
        ..claimed()
    };
    let plan = planned(&awaiting_model(), &facts);
    assert_eq!(
        operation(&plan),
        &Operation::CommitTurn(CommitTurn {
            turn: 0,
            attempt: 1,
            text: Some("Done.".to_owned()),
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_input_tokens: 4,
                cache_write_input_tokens: 2,
            },
            calls: Vec::new(),
            next: Continuation::Done,
            ending: Some(RunEnding::Completed {
                output: "Done.".to_owned(),
            }),
            silent: None,
        })
    );
    assert_eq!(guards(&plan), releases(1));
    assert_eq!(operation(&plan).status_after(RunStatus::Running), RunStatus::Completed);
}

#[test]
fn each_call_is_looked_up_in_the_frozen_offer_and_decided() {
    let limits = Limits {
        max_calls_per_turn: 4,
        ..Limits::default()
    };
    let reply = asks(&[
        ("a", WORK_LIST),
        ("b", PERSONAL_CREATE),
        ("c", WORK_CREATE),
        ("d", "create_task"),
        ("e", WORK_CREATE),
    ]);
    let snapshot = awaiting_model();
    let wanted = calls_to_decide(&snapshot, &completion(reply.clone()), &limits).expect("calls");
    let positions: Vec<(u32, &str)> = wanted
        .iter()
        .map(|(position, tool)| (*position, tool.name.as_str()))
        .collect();
    assert_eq!(positions, [(0, WORK_LIST), (1, PERSONAL_CREATE), (2, WORK_CREATE)]);
    let facts = Facts {
        model: Some(answered(0, 1, reply)),
        decisions: vec![
            (0, allow()),
            (1, approve()),
            (2, deny("Your organization blocks create_task.")),
        ],
        call_ids: (1..=5).map(call_id).collect(),
        limits,
        ..claimed()
    };
    let plan = planned(&snapshot, &facts);
    let Operation::CommitTurn(commit) = operation(&plan) else {
        panic!("commits the turn");
    };
    assert_eq!(guards(&plan), keeps(1));
    assert_eq!(commit.text.as_deref(), Some("On it."));
    assert_eq!(commit.next, Continuation::NeedCall { turn: 0, position: 0 });
    let states: Vec<CallState> = commit.calls.iter().map(|call| call.state).collect();
    assert_eq!(
        states,
        [
            CallState::Allowed,
            CallState::AwaitingApproval,
            CallState::Refused,
            CallState::Refused,
            CallState::Refused
        ]
    );
    let ids: Vec<ToolCallId> = commit.calls.iter().map(|call| call.id).collect();
    assert_eq!(ids, (1..=5).map(call_id).collect::<Vec<_>>());
    // Bound to exactly the installation its name means, with its digest.
    let personal_call = &commit.calls[1];
    let target = personal_call.target.as_ref().expect("a target");
    assert_eq!(target.installation_id, Some(personal()));
    assert_eq!(
        (target.installation_short_id.as_str(), target.tool.as_str()),
        ("p3x7ta", "create_task")
    );
    assert_eq!(
        personal_call.call_digest,
        Some(call_digest(
            personal(),
            "taskboard",
            "create_task",
            &Sha256Digest::of(b"create_task"),
            &arguments()
        ))
    );
    assert_eq!(personal_call.decision, Some(approve()));
    // Denied by the policy.
    assert_eq!(
        commit.calls[2].failure.as_ref().map(|f| f.code.as_str()),
        Some("tool_refused")
    );
    assert_eq!(
        commit.calls[2].outcome.as_ref().map(|o| o.content.as_str()),
        Some("Your organization blocks create_task.")
    );
    // A bare tool name isn't parsed into anything: no target, nothing sent.
    let unknown = &commit.calls[3];
    assert_eq!((unknown.target.as_ref(), unknown.call_digest), (None, None));
    assert_eq!(
        unknown.outcome.as_ref().map(|o| o.content.as_str()),
        Some("There's no tool named create_task available to you.")
    );
    // Past the call limit.
    assert_eq!(
        commit.calls[4].outcome.as_ref().map(|o| o.content.as_str()),
        Some("Only 4 calls can be made in one turn.")
    );
    assert!(commit.calls[4].target.is_some());
}

#[test]
fn a_turn_whose_calls_are_all_refused_moves_to_the_next_turn() {
    let facts = Facts {
        model: Some(answered(0, 1, asks(&[("a", "nope"), ("b", WORK_LIST)]))),
        decisions: vec![(1, deny("No."))],
        call_ids: vec![call_id(1), call_id(2)],
        ..claimed()
    };
    let Operation::CommitTurn(commit) = operation(&planned(&awaiting_model(), &facts)).clone() else {
        panic!("commits the turn");
    };
    assert_eq!(commit.next, Continuation::NeedModel { turn: 1 });
}

#[test]
fn every_later_attempt_offers_the_tools_frozen_at_the_first() {
    // Turn 0 offered only Work's list_tasks. Since then, a grant was edited
    // and the installation renamed: the driver's offer now differs, and the
    // next turn still offers exactly what the first did.
    let frozen = build_offer([candidate(work(), "k7m2qd", "Work", "list_tasks")]).offer;
    let mut snapshot = with_calls(
        Continuation::NeedModel { turn: 1 },
        vec![record(1, 0, WORK_LIST, CallState::Completed)],
    );
    snapshot.model_attempts[0].offer = frozen.clone();
    let changed = build_offer([
        candidate(work(), "k7m2qd", "Renamed", "create_task"),
        candidate(personal(), "p3x7ta", "Personal", "create_task"),
    ])
    .offer;
    for offer in [Some(changed), None] {
        let facts = Facts { offer, ..claimed() };
        let plan = planned(&snapshot, &facts);
        let Io::CallModel(request) = plan.io() else {
            panic!("calls the model");
        };
        assert_eq!(request.tools, frozen.specs());
        let Operation::PrepareModel(prepare) = operation(&plan) else {
            panic!("prepares an attempt");
        };
        assert_eq!((prepare.turn, prepare.attempt), (1, 1));
        assert_eq!(prepare.offer, frozen, "recorded again with the attempt");
    }
    // An attempt asked again after its worker stopped offers its own tools.
    let snapshot = awaiting_model();
    let facts = Facts {
        offer: Some(build_offer([candidate(personal(), "p3x7ta", "Personal", "create_task")]).offer),
        ..claimed()
    };
    let Operation::PrepareModel(prepare) = operation(&planned(&snapshot, &facts)).clone() else {
        panic!("prepares a new attempt");
    };
    assert_eq!((prepare.attempt, prepare.abandons), (2, Some(1)));
    assert_eq!(prepare.offer, offer());
    assert_eq!(frozen_offer(&snapshot), Some(&offer()));
    assert_eq!(frozen_offer(&fresh()), None, "built for the first attempt");
}

#[test]
fn only_the_frozen_offer_names_tools() {
    // The driver's offer for a later turn has a tool the frozen one hasn't:
    // a call to it in this turn has no target.
    let mut snapshot = awaiting_model();
    let frozen = build_offer([candidate(work(), "k7m2qd", "Work", "list_tasks")]).offer;
    snapshot.model_attempts[0].offer = frozen.clone();
    snapshot.model_attempts[0].request_digest = model_request(&snapshot, 0, 1, None, (&frozen, None), None)
        .expect("a request")
        .digest();
    let facts = Facts {
        model: Some(answered(0, 1, asks(&[("a", WORK_CREATE)]))),
        call_ids: vec![call_id(1)],
        ..claimed()
    };
    let Operation::CommitTurn(commit) = operation(&planned(&snapshot, &facts)).clone() else {
        panic!("commits the turn");
    };
    assert_eq!(commit.calls[0].target, None);
    assert_eq!(commit.calls[0].state, CallState::Refused);
}

#[test]
fn a_turn_that_times_out_twice_fails_and_says_why_the_last_attempt_went_unanswered() {
    let answer = |why, retry_after, attempt| Facts {
        model: Some(ModelAnswer {
            turn: 0,
            attempt,
            result: ModelResult::Unavailable(Unanswered { why, retry_after }),
        }),
        ..claimed()
    };
    // A first timeout yields like any unanswered attempt, marked as one,
    // and a provider's wait goes with it.
    let snapshot = awaiting_model();
    let first = answer(UnansweredWhy::TimedOut, None, 1);
    assert_eq!(
        operation(&planned(&snapshot, &first)),
        &Operation::Yield(Yield {
            why: YieldReason::ModelUnavailable {
                turn: 0,
                attempt: 1,
                timed_out: true,
                retry_after_secs: None
            },
        })
    );
    assert_eq!(
        operation(&planned(
            &snapshot,
            &answer(UnansweredWhy::NoAnswer, Some(Duration::from_secs(30)), 1)
        )),
        &Operation::Yield(Yield {
            why: YieldReason::ModelUnavailable {
                turn: 0,
                attempt: 1,
                timed_out: false,
                retry_after_secs: Some(30)
            },
        })
    );

    // The next attempt of the same turn timing out too fails the run, not
    // retried again: each may have been billed.
    let mut run = snapshot.clone();
    step(&mut run, &first);
    assert!(run.model_attempts[0].timed_out);
    step(&mut run, &facts());
    step(
        &mut run,
        &Facts {
            claim: Some(2),
            ..facts()
        },
    );
    assert_eq!(
        run.control.continuation,
        Continuation::AwaitModel { turn: 0, attempt: 2 }
    );
    let second = Facts {
        claim: Some(2),
        ..answer(UnansweredWhy::TimedOut, None, 2)
    };
    let plan = planned(&run, &second);
    assert_eq!(failure_code(&plan), MODEL_FAILED);
    // Another kind of no answer after a timeout is tried again as usual.
    assert!(matches!(
        operation(&planned(
            &run,
            &Facts {
                claim: Some(2),
                ..answer(UnansweredWhy::NoAnswer, None, 2)
            }
        )),
        Operation::Yield(_)
    ));

    // On the job's last attempt, the failure says what went wrong.
    for (why, code, says) in [
        (UnansweredWhy::NoAnswer, MODEL_UNAVAILABLE, "did not respond in time"),
        (UnansweredWhy::TimedOut, MODEL_UNAVAILABLE, "did not respond in time"),
        (UnansweredWhy::KeyReplaced, MODEL_UNAVAILABLE, "key was replaced"),
        (UnansweredWhy::Internal, "run_unavailable", "key couldn't be read"),
    ] {
        let last = Facts {
            last_attempt: true,
            ..answer(why, None, 1)
        };
        let plan = planned(&snapshot, &last);
        let Operation::Finish(Finish {
            ending: RunEnding::Failed { error },
            ..
        }) = operation(&plan)
        else {
            panic!("{why:?} fails the run");
        };
        assert_eq!(error.code, code, "{why:?}");
        assert!(error.message.contains(says), "{why:?}: {}", error.message);
    }
}

#[test]
fn a_failed_or_unusable_answer_fails_the_run_and_an_unavailable_one_yields() {
    let snapshot = awaiting_model();
    let unavailable = Facts {
        model: Some(ModelAnswer {
            turn: 0,
            attempt: 1,
            result: ModelResult::Unavailable(Unanswered::because(UnansweredWhy::NoAnswer)),
        }),
        ..claimed()
    };
    let plan = planned(&snapshot, &unavailable);
    assert_eq!(
        operation(&plan),
        &Operation::Yield(Yield {
            why: YieldReason::ModelUnavailable {
                turn: 0,
                attempt: 1,
                timed_out: false,
                retry_after_secs: None
            },
        })
    );
    // The claim is released at once, and the run asks this turn again.
    assert_eq!(guards(&plan), releases(1));
    assert_eq!(
        operation(&plan).continuation_after(snapshot.control.continuation),
        Continuation::NeedModel { turn: 0 }
    );
    let mut run = snapshot.clone();
    step(&mut run, &unavailable);
    assert_eq!(run.model_attempts[0].state, ModelAttemptState::Unanswered);
    assert_eq!(run.control.lease_until, None);
    let again = step(&mut run, &facts());
    assert!(matches!(operation(&again), Operation::Claim(_)), "a fresh claim first");
    let asked = step(
        &mut run,
        &Facts {
            claim: Some(2),
            ..facts()
        },
    );
    assert!(matches!(
        operation(&asked),
        Operation::PrepareModel(PrepareModel {
            turn: 0,
            attempt: 2,
            abandons: None,
            ..
        })
    ));
    // On the job's last attempt the run fails at once, the attempt
    // abandoned, rather than waiting for the sweep.
    let last = Facts {
        last_attempt: true,
        ..unavailable.clone()
    };
    let plan = planned(&snapshot, &last);
    assert_eq!(failure_code(&plan), MODEL_UNAVAILABLE);
    assert_eq!(guards(&plan), releases(1));
    assert!(matches!(
        operation(&plan),
        Operation::Finish(Finish {
            abandons: Some(ModelAttemptRef { turn: 0, attempt: 1 }),
            ..
        })
    ));
    let mut ended = snapshot.clone();
    step(&mut ended, &last);
    assert_eq!(ended.control.status, RunStatus::Failed);
    assert_eq!(ended.model_attempts[0].state, ModelAttemptState::Abandoned);
    // An answer on the last attempt is taken as any other.
    let answered_last = Facts {
        last_attempt: true,
        model: Some(ModelAnswer {
            turn: 0,
            attempt: 1,
            result: ModelResult::Answered(Completion {
                reply: Reply::Text("Done.".to_owned()),
                usage: TokenUsage::default(),
            }),
        }),
        ..claimed()
    };
    assert!(matches!(
        operation(&planned(&snapshot, &answered_last)),
        Operation::CommitTurn(_)
    ));
    let failed = Facts {
        model: Some(ModelAnswer {
            turn: 0,
            attempt: 1,
            result: ModelResult::Failed(Failure::new("model_unavailable", "Not served.")),
        }),
        ..claimed()
    };
    assert_eq!(failure_code(&planned(&snapshot, &failed)), "model_unavailable");
    for reply in [
        // Calls without a call.
        Reply::ToolCalls {
            text: None,
            calls: Vec::new(),
        },
        // The same call ID twice.
        asks(&[("a", WORK_LIST), ("a", WORK_LIST)]),
    ] {
        let unusable = Facts {
            model: Some(answered(0, 1, reply)),
            call_ids: vec![call_id(1), call_id(2)],
            decisions: vec![(0, allow()), (1, allow())],
            ..claimed()
        };
        assert_eq!(failure_code(&planned(&snapshot, &unusable)), "model_failed");
    }
}

#[test]
fn await_model_refuses_what_does_not_fit() {
    let snapshot = awaiting_model();
    let reply = asks(&[("a", WORK_LIST), ("b", WORK_CREATE)]);
    let with = |model: ModelAnswer, decisions: Vec<(u32, CallDecision)>, call_ids: Vec<ToolCallId>| Facts {
        model: Some(model),
        decisions,
        call_ids,
        ..claimed()
    };
    let cases = [
        (
            with(answered(0, 2, reply.clone()), Vec::new(), Vec::new()),
            AdvanceError::InvalidFact("an answer to another model attempt"),
        ),
        (
            with(answered(1, 1, reply.clone()), Vec::new(), Vec::new()),
            AdvanceError::InvalidFact("an answer to another model attempt"),
        ),
        (
            with(
                answered(0, 1, reply.clone()),
                vec![(0, allow()), (1, allow())],
                vec![call_id(1)],
            ),
            AdvanceError::MissingFact("call_ids"),
        ),
        (
            with(
                answered(0, 1, reply.clone()),
                vec![(0, allow())],
                vec![call_id(1), call_id(2)],
            ),
            AdvanceError::MissingFact("decisions"),
        ),
        (
            with(
                answered(0, 1, reply.clone()),
                vec![(0, allow()), (1, allow())],
                vec![call_id(1), call_id(1)],
            ),
            AdvanceError::InvalidFact("a call ID given twice or already used"),
        ),
    ];
    for (facts, error) in cases {
        assert_eq!(advance(&snapshot, &facts), Err(error));
    }
    // A call ID the run already has.
    let mut used = awaiting_model();
    used.turns.clear();
    used.calls.push(record(1, 0, WORK_LIST, CallState::Completed));
    let facts = with(
        answered(0, 1, reply.clone()),
        vec![(0, allow()), (1, allow())],
        vec![call_id(1), call_id(2)],
    );
    assert!(advance(&used, &facts).is_err());
    // The attempt isn't prepared any more.
    let mut answered_already = awaiting_model();
    answered_already.model_attempts[0].state = ModelAttemptState::Answered;
    assert!(matches!(
        advance(&answered_already, &claimed()),
        Err(AdvanceError::InvalidSnapshot(_))
    ));
    let mut missing = awaiting_model();
    missing.model_attempts.clear();
    assert!(matches!(
        advance(&missing, &claimed()),
        Err(AdvanceError::InvalidSnapshot(_))
    ));
    // The conversation changed under it.
    let mut changed = awaiting_model();
    changed.input = "Something else.".to_owned();
    let facts = with(answered(0, 1, Reply::Text("Done.".to_owned())), Vec::new(), Vec::new());
    assert!(matches!(
        advance(&changed, &facts),
        Err(AdvanceError::InvalidSnapshot(_))
    ));
    // Answers outside `AwaitModel` aren't decided.
    assert!(calls_to_decide(&fresh(), &completion(reply), &Limits::default()).is_err());
}

// ---------------------------------------------------------------------------
// NeedCall

/// A call waiting for approval checked before the run parks on it: its
/// installation as `installation` finds it, its recorded decision.
fn before_parking(call: &CallRecord, installation: InstallationCheck) -> Facts {
    Facts {
        send_check: Some(SendCheck {
            installation,
            ..check(call, approve())
        }),
        ..claimed()
    }
}

#[test]
fn a_call_awaiting_approval_parks_the_run() {
    let snapshot = with_calls(
        Continuation::NeedCall { turn: 0, position: 0 },
        vec![record(1, 0, WORK_CREATE, CallState::AwaitingApproval)],
    );
    // Its installation is checked first; one that is there, even if it
    // needs a reconnect or can't be reached now, is asked about.
    for installation in [InstallationCheck::ReconnectRequired, InstallationCheck::Unusable] {
        assert!(matches!(
            operation(&planned(&snapshot, &before_parking(&snapshot.calls[0], installation))),
            Operation::Park(_)
        ));
    }
    let plan = planned(
        &snapshot,
        &before_parking(&snapshot.calls[0], InstallationCheck::Usable),
    );
    assert_eq!(
        operation(&plan),
        &Operation::Park(Park {
            call_id: call_id(1),
            decision: None,
        })
    );
    assert_eq!(guards(&plan), releases(1));
    assert_eq!(
        operation(&plan).continuation_after(snapshot.control.continuation),
        Continuation::AwaitApproval { call_id: call_id(1) }
    );
    assert_eq!(
        operation(&plan).status_after(RunStatus::Running),
        RunStatus::WaitingForApproval
    );
}

#[test]
fn a_call_awaiting_approval_on_an_installation_that_went_is_refused_not_parked() {
    let snapshot = with_calls(
        Continuation::NeedCall { turn: 0, position: 0 },
        vec![
            record(1, 0, WORK_CREATE, CallState::AwaitingApproval),
            record(2, 1, PERSONAL_CREATE, CallState::Allowed),
        ],
    );
    let call = &snapshot.calls[0];
    for (installation, code, message) in [
        (
            InstallationCheck::Removed,
            INSTALLATION_REMOVED,
            "Work was removed, so this call wasn't made.",
        ),
        (
            InstallationCheck::Disconnected,
            INSTALLATION_DISCONNECTED,
            "Work was disconnected, so this call wasn't made.",
        ),
    ] {
        let plan = planned(&snapshot, &before_parking(call, installation));
        assert_eq!(
            operation(&plan),
            &Operation::AuthorizeCall(AuthorizeCall {
                call_id: call_id(1),
                decision: approve(),
                authorization: Authorization::Refuse {
                    outcome: CallOutcome {
                        content: message.to_owned(),
                        is_error: true,
                    },
                    failure: Failure::new(code, message),
                },
                next: Continuation::NeedCall { turn: 0, position: 1 },
            }),
            "{installation:?}"
        );
        assert_eq!(guards(&plan), keeps(1), "the worker keeps the run");
        assert_eq!(plan.io(), &Io::Nothing, "nothing is sent");
    }
    // The check is required, and must be of this call.
    assert_eq!(
        advance(&snapshot, &claimed()),
        Err(AdvanceError::MissingFact("send_check"))
    );
    let other = Facts {
        send_check: Some(SendCheck {
            call_id: call_id(2),
            ..check(call, approve())
        }),
        ..claimed()
    };
    assert_eq!(
        advance(&snapshot, &other),
        Err(AdvanceError::InvalidFact("a send check for another call"))
    );
}

#[test]
fn an_allowed_call_is_authorized_and_sent_to_its_own_installation() {
    let call = record(1, 0, PERSONAL_CREATE, CallState::Allowed);
    let snapshot = with_calls(Continuation::NeedCall { turn: 0, position: 0 }, vec![call.clone()]);
    for (state, verdict) in [(CallState::Allowed, allow()), (CallState::Approved, approve())] {
        let mut snapshot = snapshot.clone();
        snapshot.calls[0].state = state;
        let facts = Facts {
            send_check: Some(check(&call, verdict.clone())),
            invocation_id: Some(invocation(1)),
            ..claimed()
        };
        let plan = planned(&snapshot, &facts);
        assert_eq!(
            operation(&plan),
            &Operation::AuthorizeCall(AuthorizeCall {
                call_id: call.id,
                decision: verdict,
                authorization: Authorization::Send {
                    attempt: 1,
                    dispatch_epoch: 1,
                    invocation_id: invocation(1),
                    send_lease_ms: Limits::default().send_lease_ms,
                },
                next: Continuation::AwaitCall {
                    call_id: call.id,
                    attempt: 1,
                },
            }),
            "{state:?}"
        );
        assert_eq!(guards(&plan), keeps(1));
        assert_eq!(
            plan.io(),
            &Io::SendCall(SendRequest {
                organization_id: organization(),
                workspace_id: workspace(),
                call_id: call.id,
                attempt: 1,
                invocation_id: invocation(1),
                installation_id: personal(),
                tool: "create_task".to_owned(),
                arguments: arguments(),
            })
        );
    }
}

#[test]
fn an_allowed_call_that_needs_approval_now_parks_with_the_new_decision() {
    let call = record(1, 0, WORK_CREATE, CallState::Allowed);
    let snapshot = with_calls(Continuation::NeedCall { turn: 0, position: 0 }, vec![call.clone()]);
    let facts = Facts {
        send_check: Some(check(&call, approve())),
        invocation_id: Some(invocation(1)),
        ..claimed()
    };
    assert_eq!(
        operation(&planned(&snapshot, &facts)),
        &Operation::Park(Park {
            call_id: call.id,
            decision: Some(approve()),
        })
    );
}

#[test]
fn a_call_is_refused_at_send_with_its_reason() {
    let call = record(1, 0, WORK_CREATE, CallState::Approved);
    let later = record(2, 1, WORK_LIST, CallState::Refused);
    let last = record(3, 2, WORK_LIST, CallState::Allowed);
    let snapshot = with_calls(
        Continuation::NeedCall { turn: 0, position: 0 },
        vec![call.clone(), later, last],
    );
    let changed = SendCheck {
        definition_sha256: Some(Sha256Digest::of(b"changed")),
        ..check(&call, allow())
    };
    let gone = SendCheck {
        definition_sha256: None,
        ..check(&call, allow())
    };
    let installation = |installation| SendCheck {
        installation,
        ..check(&call, allow())
    };
    let cases = [
        (
            check(&call, deny("Your organization blocks create_task.")),
            "tool_refused",
        ),
        (changed, "tool_changed"),
        (gone, "tool_changed"),
        (installation(InstallationCheck::Removed), "installation_removed"),
        (
            installation(InstallationCheck::Disconnected),
            "installation_disconnected",
        ),
        (installation(InstallationCheck::ReconnectRequired), RECONNECT_REQUIRED),
        (installation(InstallationCheck::Unusable), "tool_refused"),
    ];
    for (send_check, code) in cases {
        let facts = Facts {
            send_check: Some(send_check),
            invocation_id: Some(invocation(1)),
            ..claimed()
        };
        let plan = planned(&snapshot, &facts);
        let Operation::AuthorizeCall(AuthorizeCall {
            authorization: Authorization::Refuse { outcome, failure },
            next,
            ..
        }) = operation(&plan)
        else {
            panic!("refuses for {code}");
        };
        assert_eq!(failure.code, code);
        assert!(outcome.is_error);
        assert_eq!(outcome.content, failure.message);
        // On to the next open call, skipping the settled one.
        assert_eq!(*next, Continuation::NeedCall { turn: 0, position: 2 });
        assert_eq!(plan.io(), &Io::Nothing, "nothing is sent");
    }
    let facts = Facts {
        send_check: Some(SendCheck {
            installation: InstallationCheck::Removed,
            ..check(&call, allow())
        }),
        ..claimed()
    };
    let plan = planned(&snapshot, &facts);
    let Operation::AuthorizeCall(AuthorizeCall {
        authorization: Authorization::Refuse { outcome, .. },
        ..
    }) = operation(&plan)
    else {
        panic!("refuses");
    };
    assert_eq!(outcome.content, "Work was removed, so this call wasn't made.");

    // A definition changed or gone since the run offered the tool (a change
    // accepted mid-run, say): the tool is no longer there for this run, a
    // fixed message the model has no reason to try again on.
    for send_check in [
        SendCheck {
            definition_sha256: Some(Sha256Digest::of(b"changed")),
            ..check(&call, allow())
        },
        SendCheck {
            definition_sha256: None,
            ..check(&call, allow())
        },
    ] {
        let facts = Facts {
            send_check: Some(send_check),
            invocation_id: Some(invocation(1)),
            ..claimed()
        };
        let plan = planned(&snapshot, &facts);
        let Operation::AuthorizeCall(AuthorizeCall {
            authorization: Authorization::Refuse { outcome, failure },
            ..
        }) = operation(&plan)
        else {
            panic!("refuses");
        };
        assert_eq!(failure.code, "tool_changed");
        assert_eq!(outcome.content, "create_task is no longer available in this run.");
    }
}

#[test]
fn the_last_call_refused_moves_to_the_next_turn() {
    let call = record(1, 0, WORK_CREATE, CallState::Allowed);
    let snapshot = with_calls(Continuation::NeedCall { turn: 0, position: 0 }, vec![call.clone()]);
    let facts = Facts {
        send_check: Some(check(&call, deny("No."))),
        ..claimed()
    };
    let Operation::AuthorizeCall(authorize) = operation(&planned(&snapshot, &facts)).clone() else {
        panic!("authorizes");
    };
    assert_eq!(authorize.next, Continuation::NeedModel { turn: 1 });
}

#[test]
fn need_call_refuses_what_does_not_fit() {
    let call = record(1, 0, WORK_CREATE, CallState::Allowed);
    let snapshot = with_calls(Continuation::NeedCall { turn: 0, position: 0 }, vec![call.clone()]);
    let with = |send_check: Option<SendCheck>, invocation_id: Option<InvocationId>| Facts {
        send_check,
        invocation_id,
        ..claimed()
    };
    assert_eq!(
        advance(&snapshot, &with(None, None)),
        Err(AdvanceError::MissingFact("send_check"))
    );
    let other = SendCheck {
        call_id: call_id(9),
        ..check(&call, allow())
    };
    assert_eq!(
        advance(&snapshot, &with(Some(other), Some(invocation(1)))),
        Err(AdvanceError::InvalidFact("a send check for another call"))
    );
    assert_eq!(
        advance(&snapshot, &with(Some(check(&call, allow())), None)),
        Err(AdvanceError::MissingFact("invocation_id"))
    );
    for state in [
        CallState::Started,
        CallState::Completed,
        CallState::Failed,
        CallState::Unknown,
        CallState::Denied,
        CallState::Refused,
    ] {
        let mut settled = snapshot.clone();
        settled.calls[0].state = state;
        assert!(
            matches!(advance(&settled, &claimed()), Err(AdvanceError::InvalidSnapshot(_))),
            "{state:?}"
        );
    }
    let mut no_target = snapshot.clone();
    no_target.calls[0].target = None;
    let facts = with(Some(check(&call, allow())), Some(invocation(1)));
    assert!(matches!(
        advance(&no_target, &facts),
        Err(AdvanceError::InvalidSnapshot(_))
    ));
    let mut missing = snapshot.clone();
    missing.control.continuation = Continuation::NeedCall { turn: 0, position: 1 };
    assert!(matches!(
        advance(&missing, &facts),
        Err(AdvanceError::InvalidSnapshot(_))
    ));
    let mut wrong_turn = snapshot.clone();
    wrong_turn.control.continuation = Continuation::NeedCall { turn: 1, position: 0 };
    assert!(matches!(
        advance(&wrong_turn, &facts),
        Err(AdvanceError::InvalidSnapshot(_))
    ));
    // Past the step budget it yields before anything else.
    let spent = Facts {
        steps: Limits::default().max_steps_per_claim,
        ..claimed()
    };
    assert!(matches!(
        operation(&planned(&snapshot, &spent)),
        Operation::Yield(Yield {
            why: YieldReason::StepBudget,
            ..
        })
    ));
}

// ---------------------------------------------------------------------------
// AwaitCall

#[test]
fn a_live_send_claim_is_never_settled_unknown() {
    let snapshot = sending(30_000);
    let plan = planned(&snapshot, &claimed());
    assert_eq!(
        operation(&plan),
        &Operation::Yield(Yield {
            why: YieldReason::SendInFlight {
                call_id: call_id(1),
                attempt: 1,
            },
        })
    );
    assert_eq!(guards(&plan), releases(1));
}

#[test]
fn a_lapsed_send_claim_settles_the_call_unknown() {
    let snapshot = sending(0);
    let plan = planned(&snapshot, &claimed());
    let Operation::SettleUnknown(settle) = operation(&plan) else {
        panic!("settles unknown");
    };
    assert_eq!((settle.call_id, settle.attempt), (call_id(1), 1));
    assert_eq!(settle.failure.code, "outcome_unknown");
    assert_eq!(settle.outcome.content, UNKNOWN);
    assert_eq!(settle.next, Some(Continuation::NeedCall { turn: 0, position: 1 }));
    assert_eq!(
        guards(&plan),
        Guards {
            send: Some(SendGuard {
                call_id: call_id(1),
                attempt: 1,
                dispatch_epoch: 1,
                invocation_id: invocation(1),
                lease: SendLease::Lapsed,
            }),
            ..keeps(1)
        }
    );
}

#[test]
fn await_call_refuses_what_does_not_fit() {
    let mut not_started = sending(0);
    not_started.calls[0].state = CallState::Allowed;
    assert!(matches!(
        advance(&not_started, &claimed()),
        Err(AdvanceError::InvalidSnapshot(_))
    ));
    let mut no_attempt = sending(0);
    no_attempt.calls[0].attempts.clear();
    assert!(matches!(
        advance(&no_attempt, &claimed()),
        Err(AdvanceError::InvalidSnapshot(_))
    ));
}

// ---------------------------------------------------------------------------
// The sender's outcome

fn sent(outcome: SendOutcome) -> Facts {
    Facts {
        sent: Some(SendResult {
            call_id: call_id(1),
            attempt: 1,
            invocation_id: invocation(1),
            outcome,
        }),
        ..facts()
    }
}

fn result() -> Map<String, Value> {
    let mut result = Map::new();
    result.insert("id".to_owned(), json!("task_1"));
    result
}

#[test]
fn the_senders_outcome_settles_the_call_under_its_send_claim() {
    let cases = [
        (SendOutcome::Completed { result: result() }, CallState::Completed, None),
        (
            SendOutcome::ToolError { result: result() },
            CallState::Failed,
            Some("tool_error"),
        ),
        (SendOutcome::NotSent, CallState::Failed, Some("tool_unreachable")),
        (SendOutcome::Rejected, CallState::Failed, Some("tool_rejected")),
        (SendOutcome::Withheld, CallState::Failed, Some("result_withheld")),
        (SendOutcome::Unknown, CallState::Unknown, Some("outcome_unknown")),
        (SendOutcome::Unauthorized, CallState::Failed, Some(RECONNECT_REQUIRED)),
    ];
    let snapshot = sending(30_000);
    for (outcome, state, code) in cases {
        let plan = planned(&snapshot, &sent(outcome.clone()));
        let Operation::RecordOutcome(record) = operation(&plan) else {
            panic!("records");
        };
        let settle = record.settle.as_ref().expect("settles");
        assert_eq!(settle.state, state, "{outcome:?}");
        assert_eq!(settle.failure.as_ref().map(|failure| failure.code.as_str()), code);
        assert_eq!(record.next, Some(Continuation::NeedCall { turn: 0, position: 1 }));
        // Fenced by the send claim alone: no owner, whatever the cancel flag.
        assert_eq!(
            guards(&plan),
            Guards {
                send: Some(SendGuard {
                    call_id: call_id(1),
                    attempt: 1,
                    dispatch_epoch: 1,
                    invocation_id: invocation(1),
                    lease: SendLease::Any,
                }),
                ..Guards::default()
            }
        );
    }
    let Operation::RecordOutcome(record) =
        operation(&planned(&snapshot, &sent(SendOutcome::Completed { result: result() }))).clone()
    else {
        panic!("records");
    };
    let settle = record.settle.expect("settles");
    assert_eq!(settle.outcome.content, r#"{"id":"task_1"}"#);
    assert!(!settle.outcome.is_error);
}

#[test]
fn the_senders_outcome_is_recorded_after_a_takeover_or_a_cancel() {
    // Another worker holds the run now; the sender holds no claim.
    let mut taken = sending(30_000);
    taken.control.owner_epoch = 2;
    let plan = planned(&taken, &sent(SendOutcome::Completed { result: result() }));
    assert!(matches!(operation(&plan), Operation::RecordOutcome(_)));

    // Cancelled after the send was authorized: the call is settled, the run
    // stays where it is.
    let mut cancelled = sending(30_000);
    cancelled.control.status = RunStatus::Cancelled;
    cancelled.control.continuation = Continuation::Done;
    cancelled.control.cancel_requested_at = Some(at(-5));
    cancelled.control.lease_until = None;
    cancelled.calls[1].state = CallState::Refused;
    let plan = planned(&cancelled, &sent(SendOutcome::Completed { result: result() }));
    let Operation::RecordOutcome(record) = operation(&plan) else {
        panic!("records");
    };
    assert!(record.settle.is_some());
    assert_eq!(record.next, None);
    assert_eq!(
        operation(&plan).status_after(RunStatus::Cancelled),
        RunStatus::Cancelled
    );
}

#[test]
fn a_late_outcome_is_kept_as_evidence_only() {
    let mut settled = sending(-1);
    settled.calls[0].state = CallState::Unknown;
    settled.control.continuation = Continuation::NeedCall { turn: 0, position: 1 };
    let plan = planned(&settled, &sent(SendOutcome::Completed { result: result() }));
    let Operation::RecordOutcome(record) = operation(&plan) else {
        panic!("records");
    };
    assert_eq!((record.settle.as_ref(), record.next), (None, None));
    assert_eq!(
        operation(&plan).continuation_after(settled.control.continuation),
        settled.control.continuation
    );
}

#[test]
fn an_outcome_is_recorded_once_and_only_for_its_claim() {
    let mut recorded = sending(30_000);
    recorded.calls[0].attempts[0].outcome_recorded = true;
    recorded.calls[0].state = CallState::Completed;
    assert_eq!(
        planned(&recorded, &sent(SendOutcome::Unknown)),
        Plan::Idle(Idle::Recorded)
    );
    let snapshot = sending(30_000);
    let mismatched = [
        SendResult {
            call_id: call_id(9),
            attempt: 1,
            invocation_id: invocation(1),
            outcome: SendOutcome::Unknown,
        },
        SendResult {
            call_id: call_id(1),
            attempt: 2,
            invocation_id: invocation(1),
            outcome: SendOutcome::Unknown,
        },
        SendResult {
            call_id: call_id(1),
            attempt: 1,
            invocation_id: invocation(2),
            outcome: SendOutcome::Unknown,
        },
    ];
    for result in mismatched {
        let facts = Facts {
            sent: Some(result),
            ..facts()
        };
        assert!(matches!(advance(&snapshot, &facts), Err(AdvanceError::InvalidFact(_))));
    }
    // A send claim on a call in a state no send leaves it in.
    let mut odd = sending(30_000);
    odd.calls[0].state = CallState::Allowed;
    assert!(matches!(
        advance(&odd, &sent(SendOutcome::Unknown)),
        Err(AdvanceError::InvalidSnapshot(_))
    ));
}

#[test]
fn a_call_that_needs_a_reconnect_fails_the_run_and_closes_the_rest() {
    let mut failed = record(1, 0, WORK_CREATE, CallState::Failed);
    failed.failure = Some(Failure::new(RECONNECT_REQUIRED, "Work needs to be reconnected."));
    let snapshot = with_calls(
        Continuation::NeedCall { turn: 0, position: 1 },
        vec![
            failed,
            record(2, 1, WORK_LIST, CallState::Allowed),
            record(3, 2, WORK_LIST, CallState::AwaitingApproval),
        ],
    );
    let plan = planned(&snapshot, &claimed());
    let Operation::Finish(finish) = operation(&plan) else {
        panic!("finishes");
    };
    assert_eq!(
        finish.ending,
        RunEnding::Failed {
            error: Failure::new(RECONNECT_REQUIRED, "Work needs to be reconnected.")
        }
    );
    let closing = finish.closing.as_ref().expect("closes the open calls");
    assert_eq!(closing.calls, [call_id(2), call_id(3)]);
    assert_eq!(guards(&plan), releases(1));
}

// ---------------------------------------------------------------------------
// Control operations

#[test]
fn admit_starts_every_run_the_same_way() {
    let run = NewRun {
        agent_id: AgentId::from_uuid(Uuid::from_u128(20)),
        session_id: SessionId::from_uuid(Uuid::from_u128(21)),
        trigger: RunTrigger::Api,
        input: "Make a task.".to_owned(),
        memory: MemoryMode::Off,
        model: RunModel {
            model_id: model_id(),
            connection_id: connection_id(),
            provider_model_id: "model-a".to_owned(),
        },
        instructions: "Be brief.".to_owned(),
        channel: None,
    };
    let request = admit(organization(), workspace(), run_id(), &run);
    assert_eq!(request, admit(organization(), workspace(), run_id(), &run));
    assert_eq!((request.from_revision(), request.to_revision()), (0, Some(1)));
    assert_eq!(request.guards(), &Guards::default());
    let Operation::Admit(admitted) = request.operation() else {
        panic!("admits");
    };
    assert_eq!(admitted.execution_version, EXECUTION_VERSION);
    assert_eq!(admitted.memory, MemoryMode::Off, "what the run was started with");
    assert_eq!(
        (admitted.model.as_str(), admitted.model_id, admitted.connection_id),
        ("model-a", model_id(), connection_id())
    );
    assert_eq!(request.operation().status_after(RunStatus::Queued), RunStatus::Queued);
    assert_eq!(
        request.operation().continuation_after(Continuation::Done),
        Continuation::NeedModel { turn: 0 }
    );
}

fn parked() -> Snapshot {
    let mut snapshot = with_calls(
        Continuation::AwaitApproval { call_id: call_id(2) },
        vec![
            record(1, 0, WORK_LIST, CallState::Completed),
            record(2, 1, WORK_CREATE, CallState::AwaitingApproval),
            record(3, 2, PERSONAL_CREATE, CallState::Allowed),
        ],
    );
    snapshot.control.status = RunStatus::WaitingForApproval;
    snapshot.control.lease_until = None;
    snapshot
}

#[test]
fn an_approval_is_bound_to_the_call_and_resumes_at_it() {
    let snapshot = parked();
    let request = decide(&snapshot, call_id(2), ApprovalDecision::Approved, None).expect("resolves");
    let Operation::ResolveApproval(resolve) = request.operation() else {
        panic!("resolves");
    };
    assert_eq!(resolve.call_digest, snapshot.calls[1].call_digest);
    assert_eq!(resolve.next, Continuation::NeedCall { turn: 0, position: 1 });
    assert_eq!(resolve.outcome, None);
    assert_eq!(request.guards(), &Guards::default());
    assert_eq!(
        request.operation().status_after(RunStatus::WaitingForApproval),
        RunStatus::Running
    );
}

#[test]
fn a_denial_goes_back_to_the_model_and_moves_on() {
    let snapshot = parked();
    let with_reason =
        decide(&snapshot, call_id(2), ApprovalDecision::Denied, Some("Not this week.")).expect("resolves");
    let Operation::ResolveApproval(resolve) = with_reason.operation() else {
        panic!("resolves");
    };
    assert_eq!(
        resolve.outcome.as_ref().map(|outcome| outcome.content.as_str()),
        Some("A person denied this call, so it wasn't made. Their reason: Not this week.")
    );
    assert_eq!(resolve.next, Continuation::NeedCall { turn: 0, position: 2 });
    let without = decide(&snapshot, call_id(2), ApprovalDecision::Denied, None).expect("resolves");
    let Operation::ResolveApproval(resolve) = without.operation() else {
        panic!("resolves");
    };
    assert_eq!(
        resolve.outcome.as_ref().map(|outcome| outcome.content.as_str()),
        Some(DENIED)
    );
}

#[test]
fn an_approval_answers_not_waiting_unless_the_run_waits_for_that_call() {
    let snapshot = parked();
    assert_eq!(
        decide(&snapshot, call_id(3), ApprovalDecision::Approved, None),
        Err(ControlError::NotWaiting)
    );
    let mut cancelled = parked();
    cancelled.control.status = RunStatus::Cancelled;
    cancelled.control.continuation = Continuation::Done;
    cancelled.control.cancel_requested_at = Some(at(0));
    let mut running_again = parked();
    running_again.control.status = RunStatus::Running;
    running_again.control.continuation = Continuation::NeedCall { turn: 0, position: 1 };
    let mut odd = parked();
    odd.calls[1].state = CallState::Approved;
    for snapshot in [cancelled, running_again, odd] {
        assert_eq!(
            decide(&snapshot, call_id(2), ApprovalDecision::Approved, None),
            Err(ControlError::NotWaiting)
        );
    }
}

/// The job's refusal of the call `snapshot` waits on, its installation
/// gone as `gone` says, by caller 1, in the run's own scope.
fn refuse_parked(
    snapshot: &Snapshot,
    installation_id: InstallationId,
    gone: Gone,
) -> Result<TransitionRequest, ControlError> {
    refuse_waiting(organization(), workspace(), snapshot, caller(1), installation_id, gone)
}

#[test]
fn a_call_waiting_on_an_installation_that_went_is_refused_and_the_run_moves_on() {
    let snapshot = parked();
    for (gone, code, message) in [
        (
            Gone::Removed,
            INSTALLATION_REMOVED,
            "Work was removed, so this call wasn't made.",
        ),
        (
            Gone::Disconnected,
            INSTALLATION_DISCONNECTED,
            "Work was disconnected, so this call wasn't made.",
        ),
    ] {
        let request = refuse_parked(&snapshot, work(), gone).expect("refuses");
        assert_eq!(
            request.operation(),
            &Operation::RefuseWaiting(RefuseWaiting {
                request_id: caller(1),
                call_id: call_id(2),
                installation_id: work(),
                outcome: CallOutcome {
                    content: message.to_owned(),
                    is_error: true,
                },
                failure: Failure::new(code, message),
                next: Continuation::NeedCall { turn: 0, position: 2 },
            })
        );
        assert_eq!(request.guards(), &Guards::default(), "no claim is needed");
        assert_eq!(request.from_revision(), snapshot.control.revision);
        assert_eq!(
            request.operation().status_after(RunStatus::WaitingForApproval),
            RunStatus::Running,
            "runnable again"
        );
        assert_eq!(
            request.operation().continuation_after(snapshot.control.continuation),
            Continuation::NeedCall { turn: 0, position: 2 }
        );
        // Deterministic for one request; two jobs never build the same one.
        assert_eq!(refuse_parked(&snapshot, work(), gone), Ok(request.clone()));
        let other = refuse_waiting(organization(), workspace(), &snapshot, caller(2), work(), gone).expect("refuses");
        assert_eq!(other.id(), request.id());
        assert_ne!(other.digest(), request.digest());
    }
    // The last open call of its turn: the next turn.
    let mut last = parked();
    last.calls[2].state = CallState::Completed;
    let request = refuse_parked(&last, work(), Gone::Removed).expect("refuses");
    assert_eq!(
        request.operation().continuation_after(last.control.continuation),
        Continuation::NeedModel { turn: 1 }
    );
}

#[test]
fn only_a_call_waiting_on_the_installation_that_went_is_refused() {
    let snapshot = parked();
    // Another installation's call, or another installation gone.
    assert_eq!(
        refuse_parked(&snapshot, personal(), Gone::Removed),
        Err(ControlError::NotWaiting)
    );
    let mut cancelled = parked();
    cancelled.control.status = RunStatus::Cancelled;
    cancelled.control.continuation = Continuation::Done;
    cancelled.control.cancel_requested_at = Some(at(0));
    let mut decided = parked();
    decided.control.status = RunStatus::Running;
    decided.control.continuation = Continuation::NeedCall { turn: 0, position: 1 };
    decided.calls[1].state = CallState::Approved;
    let mut odd = parked();
    odd.calls[1].state = CallState::Approved;
    let mut no_target = parked();
    no_target.calls[1].target = None;
    for snapshot in [cancelled, decided, odd, no_target] {
        assert_eq!(
            refuse_parked(&snapshot, work(), Gone::Disconnected),
            Err(ControlError::NotWaiting)
        );
    }
    // Only in the run's own organization and workspace.
    for (organization_id, workspace_id) in [
        (organization(), WorkspaceId::from_uuid(Uuid::from_u128(99))),
        (OrganizationId::from_uuid(Uuid::from_u128(99)), workspace()),
    ] {
        assert_eq!(
            refuse_waiting(
                organization_id,
                workspace_id,
                &snapshot,
                caller(1),
                work(),
                Gone::Removed
            ),
            Err(ControlError::NotFound)
        );
    }
    let mut other_version = parked();
    other_version.control.execution_version = EXECUTION_VERSION + 1;
    assert_eq!(
        refuse_parked(&other_version, work(), Gone::Removed),
        Err(ControlError::UnsupportedVersion(EXECUTION_VERSION + 1))
    );
}

#[test]
fn a_run_at_a_call_waiting_on_an_installation_may_still_park_on_it() {
    let at_call = with_calls(
        Continuation::NeedCall { turn: 0, position: 1 },
        vec![
            record(1, 0, WORK_LIST, CallState::Completed),
            record(2, 1, WORK_CREATE, CallState::AwaitingApproval),
            record(3, 2, PERSONAL_CREATE, CallState::AwaitingApproval),
        ],
    );
    assert!(may_park_on(&at_call, work()));
    // Not the call it's at, or parked already: the job finds it, or the
    // check before parking does.
    assert!(!may_park_on(&at_call, personal()));
    assert!(!may_park_on(&parked(), work()));
    let mut allowed = at_call.clone();
    allowed.calls[1].state = CallState::Allowed;
    assert!(!may_park_on(&allowed, work()));
    let mut cancelled = at_call;
    cancelled.control.status = RunStatus::Cancelled;
    cancelled.control.continuation = Continuation::Done;
    cancelled.control.cancel_requested_at = Some(at(0));
    assert!(!may_park_on(&cancelled, work()));
}

#[test]
fn cancel_closes_every_call_not_yet_sent() {
    let mut snapshot = sending(30_000);
    snapshot
        .calls
        .push(record(3, 2, WORK_CREATE, CallState::AwaitingApproval));
    snapshot.calls.push(record(4, 3, WORK_CREATE, CallState::Approved));
    snapshot.calls.push(record(5, 4, WORK_CREATE, CallState::Refused));
    let request = cancel(
        organization(),
        workspace(),
        &snapshot,
        caller(1),
        RunTrigger::Console,
        canceller(),
    )
    .expect("cancels");
    let Operation::Cancel(cancel_op) = request.operation() else {
        panic!("cancels");
    };
    assert_eq!(
        cancel_op.closing.calls,
        [call_id(2), call_id(3), call_id(4)],
        "not the one being sent"
    );
    assert_eq!(cancel_op.closing.failure.code, "run_cancelled");
    assert_eq!(request.guards(), &Guards::default(), "never waits behind the worker");
    assert_eq!(
        request.operation().status_after(RunStatus::Running),
        RunStatus::Cancelled
    );
    assert_eq!(
        request.operation().continuation_after(snapshot.control.continuation),
        Continuation::Done
    );
    for status in [RunStatus::Completed, RunStatus::Failed, RunStatus::Cancelled] {
        let mut finished = running(Continuation::Done);
        finished.control.status = status;
        assert_eq!(
            cancel(
                organization(),
                workspace(),
                &finished,
                caller(1),
                RunTrigger::Api,
                canceller()
            ),
            Err(ControlError::Finished)
        );
    }
    assert!(
        cancel(
            organization(),
            workspace(),
            &parked(),
            caller(1),
            RunTrigger::Api,
            canceller()
        )
        .is_ok()
    );
    assert!(
        cancel(
            organization(),
            workspace(),
            &fresh(),
            caller(1),
            RunTrigger::Api,
            canceller()
        )
        .is_ok()
    );
}

#[test]
fn the_sweep_fails_only_runs_no_one_holds() {
    let mut abandoned = with_calls(
        Continuation::NeedCall { turn: 0, position: 0 },
        vec![record(1, 0, WORK_CREATE, CallState::Allowed)],
    );
    abandoned.control.lease_until = Some(at(-1));
    let request = fail_abandoned(organization(), workspace(), &abandoned, caller(1), at(0)).expect("fails it");
    let Operation::FailAbandoned(failed) = request.operation() else {
        panic!("fails");
    };
    assert_eq!(failed.error.code, "run_unavailable");
    assert_eq!(
        failed.closing.as_ref().map(|closing| closing.calls.clone()),
        Some(vec![call_id(1)])
    );
    assert_eq!(
        request.guards(),
        &Guards {
            no_live_claim: true,
            no_live_send: true,
            ..Guards::default()
        }
    );
    assert!(fail_abandoned(organization(), workspace(), &fresh(), caller(1), at(0)).is_ok());
    assert_eq!(
        fail_abandoned(
            organization(),
            workspace(),
            &running(Continuation::NeedModel { turn: 0 }),
            caller(1),
            at(0)
        ),
        Err(ControlError::Claimed)
    );
    assert_eq!(
        fail_abandoned(organization(), workspace(), &parked(), caller(1), at(0)),
        Err(ControlError::NotAbandoned)
    );
    let mut finished = running(Continuation::Done);
    finished.control.status = RunStatus::Completed;
    assert_eq!(
        fail_abandoned(organization(), workspace(), &finished, caller(1), at(0)),
        Err(ControlError::Finished)
    );
}

// ---------------------------------------------------------------------------
// Operations and determinism

#[test]
fn every_operation_says_where_it_leaves_the_run() {
    let call = call_id(1);
    let before = Continuation::NeedCall { turn: 0, position: 0 };
    let closing = Closing {
        calls: Vec::new(),
        outcome: refused("x".to_owned()),
        failure: Failure::new("x", "x"),
    };
    let failed = RunEnding::Failed {
        error: Failure::new("x", "x"),
    };
    let next = Continuation::NeedModel { turn: 1 };
    let cases: Vec<(Operation, RunStatus, Continuation)> = vec![
        (
            Operation::Claim(Claim {
                epoch: 1,
                claimant: claimant(1),
                lease_ms: 1,
                starts: true,
            }),
            RunStatus::Running,
            before,
        ),
        (
            Operation::Claim(Claim {
                epoch: 2,
                claimant: claimant(2),
                lease_ms: 1,
                starts: false,
            }),
            RunStatus::Queued,
            before,
        ),
        (
            Operation::PrepareModel(PrepareModel {
                turn: 1,
                attempt: 2,
                abandons: None,
                request_digest: Sha256Digest::of(b""),
                offer: Offer::default(),
                memory: None,
                key_version: None,
                steer: None,
            }),
            RunStatus::Queued,
            Continuation::AwaitModel { turn: 1, attempt: 2 },
        ),
        (
            Operation::CommitTurn(CommitTurn {
                turn: 0,
                attempt: 1,
                text: None,
                usage: Usage::default(),
                calls: Vec::new(),
                next,
                ending: None,
                silent: None,
            }),
            RunStatus::Running,
            next,
        ),
        (
            Operation::CommitTurn(CommitTurn {
                turn: 0,
                attempt: 1,
                text: None,
                usage: Usage::default(),
                calls: Vec::new(),
                next: Continuation::Done,
                ending: Some(failed.clone()),
                silent: None,
            }),
            RunStatus::Failed,
            Continuation::Done,
        ),
        (
            Operation::Park(Park {
                call_id: call,
                decision: None,
            }),
            RunStatus::WaitingForApproval,
            Continuation::AwaitApproval { call_id: call },
        ),
        (
            Operation::AuthorizeCall(AuthorizeCall {
                call_id: call,
                decision: allow(),
                authorization: Authorization::Refuse {
                    outcome: refused("x".to_owned()),
                    failure: Failure::new("x", "x"),
                },
                next,
            }),
            RunStatus::Queued,
            next,
        ),
        (
            Operation::RefuseWaiting(RefuseWaiting {
                request_id: caller(1),
                call_id: call,
                installation_id: work(),
                outcome: refused("x".to_owned()),
                failure: Failure::new("x", "x"),
                next,
            }),
            RunStatus::Running,
            next,
        ),
        (
            Operation::RecordOutcome(RecordOutcome {
                call_id: call,
                attempt: 1,
                invocation_id: invocation(1),
                outcome: SendOutcome::Unknown,
                settle: None,
                next: None,
            }),
            RunStatus::Queued,
            before,
        ),
        (
            Operation::SettleUnknown(SettleUnknown {
                call_id: call,
                attempt: 1,
                outcome: refused("x".to_owned()),
                failure: Failure::new("x", "x"),
                next: Some(next),
            }),
            RunStatus::Queued,
            next,
        ),
        (
            Operation::Cancel(Cancel {
                request_id: caller(1),
                source: RunTrigger::Api,
                actor: canceller(),
                closing: closing.clone(),
                abandons: None,
            }),
            RunStatus::Cancelled,
            Continuation::Done,
        ),
        (
            Operation::Yield(Yield {
                why: YieldReason::StepBudget,
            }),
            RunStatus::Queued,
            before,
        ),
        (
            Operation::Yield(Yield {
                why: YieldReason::ModelUnavailable {
                    turn: 2,
                    attempt: 1,
                    timed_out: false,
                    retry_after_secs: None,
                },
            }),
            RunStatus::Queued,
            Continuation::NeedModel { turn: 2 },
        ),
        (
            Operation::Finish(Finish {
                ending: RunEnding::Completed { output: "x".to_owned() },
                closing: None,
                abandons: None,
            }),
            RunStatus::Completed,
            Continuation::Done,
        ),
        (
            Operation::FailAbandoned(FailAbandoned {
                request_id: caller(1),
                error: Failure::new("x", "x"),
                closing: Some(closing),
                abandons: None,
            }),
            RunStatus::Failed,
            Continuation::Done,
        ),
    ];
    for (operation, status, continuation) in cases {
        assert_eq!(
            operation.status_after(RunStatus::Queued),
            status,
            "{}",
            operation.name()
        );
        assert_eq!(
            operation.continuation_after(before),
            continuation,
            "{}",
            operation.name()
        );
    }
}

#[test]
fn time_never_enters_a_request() {
    // The same step planned at two times is the same request: the store
    // reads its own clock.
    let snapshot = running(Continuation::NeedModel { turn: 0 });
    let mut later = claimed();
    later.now = at(30_000);
    assert_eq!(planned(&snapshot, &claimed()), planned(&snapshot, &later));
    let fresh_run = fresh();
    let mut later = facts();
    later.now = at(1_000_000);
    assert_eq!(planned(&fresh_run, &facts()), planned(&fresh_run, &later));
}

// ---------------------------------------------------------------------------
// Whole runs, through a model of the store

/// What a store does with a committed request, minus SQL: enough to walk a
/// run through `advance`. Checks the revision and the guards as the store
/// must.
fn commit(snapshot: &mut Snapshot, request: &TransitionRequest, now: Timestamp) {
    let control = &mut snapshot.control;
    assert_eq!(request.from_revision(), control.revision, "the expected revision");
    let guards = request.guards();
    if let Some(owner) = guards.owner {
        assert_eq!(owner.epoch, control.owner_epoch, "the owner epoch");
        assert_eq!(Some(owner.claimant), control.owner_claimant, "the owner claimant");
        assert!(control.lease_until.is_some_and(|until| until > now), "a live lease");
    }
    if guards.not_cancelled {
        assert_eq!(control.cancel_requested_at, None, "not cancelled");
    }
    if guards.no_live_claim {
        assert!(control.lease_until.is_none_or(|until| until <= now), "no live claim");
    }
    if guards.no_live_send {
        assert!(
            sends_in_flight(snapshot).all(|(_, attempt)| attempt.send_lease_until <= now),
            "no live send"
        );
    }
    if let Some(send) = guards.send {
        let call = snapshot
            .calls
            .iter()
            .find(|call| call.id == send.call_id)
            .expect("the call");
        let attempt = call.attempt(send.attempt).expect("the attempt");
        assert_eq!(
            (attempt.dispatch_epoch, attempt.invocation_id),
            (send.dispatch_epoch, send.invocation_id)
        );
        if send.lease == SendLease::Lapsed {
            assert!(attempt.send_lease_until <= now, "a lapsed send lease");
        }
    }
    let after = |ms: u64| Timestamp::from_millis(now.millis() + i64::try_from(ms).expect("small"));
    let operation = request.operation();
    let call = |snapshot: &mut Snapshot, id: ToolCallId| -> usize {
        snapshot.calls.iter().position(|call| call.id == id).expect("the call")
    };
    let close = |snapshot: &mut Snapshot, closing: &Closing| {
        for id in &closing.calls {
            let index = call(snapshot, *id);
            let record = &mut snapshot.calls[index];
            assert!(record.is_open(), "only open calls close");
            record.state = CallState::Refused;
            record.outcome = Some(closing.outcome.clone());
            record.failure = Some(closing.failure.clone());
        }
    };
    match operation {
        Operation::Admit(_) => panic!("the run exists"),
        Operation::Claim(claim) => {
            snapshot.control.owner_epoch = claim.epoch;
            snapshot.control.owner_claimant = Some(claim.claimant);
            snapshot.control.lease_until = Some(after(claim.lease_ms));
        }
        Operation::PrepareModel(prepare) => {
            if let Some(abandoned) = prepare.abandons {
                for attempt in &mut snapshot.model_attempts {
                    if attempt.turn == prepare.turn && attempt.attempt == abandoned {
                        attempt.state = ModelAttemptState::Abandoned;
                    }
                }
            }
            snapshot.model_attempts.push(ModelAttempt {
                turn: prepare.turn,
                attempt: prepare.attempt,
                state: ModelAttemptState::Prepared,
                request_digest: prepare.request_digest,
                offer: prepare.offer.clone(),
                memory: None,
                timed_out: false,
            });
        }
        Operation::CommitTurn(commit) => {
            for attempt in &mut snapshot.model_attempts {
                if attempt.turn == commit.turn && attempt.attempt == commit.attempt {
                    attempt.state = ModelAttemptState::Answered;
                }
            }
            snapshot.turns.push(CommittedTurn {
                turn: commit.turn,
                text: commit.text.clone(),
            });
            for new in &commit.calls {
                snapshot.calls.push(CallRecord {
                    id: new.id,
                    turn: commit.turn,
                    position: new.position,
                    model_call_id: new.model_call_id.clone(),
                    model_tool_name: new.model_tool_name.clone(),
                    target: new.target.clone(),
                    arguments: new.arguments.clone(),
                    state: new.state,
                    decision: new.decision.clone(),
                    call_digest: new.call_digest,
                    approval: None,
                    outcome: new.outcome.clone(),
                    failure: new.failure.clone(),
                    attempts: Vec::new(),
                });
            }
        }
        Operation::Park(park) => {
            let index = call(snapshot, park.call_id);
            let record = &mut snapshot.calls[index];
            record.state = CallState::AwaitingApproval;
            if let Some(decision) = &park.decision {
                record.decision = Some(decision.clone());
            }
        }
        Operation::ResolveApproval(resolve) => {
            let index = call(snapshot, resolve.call_id);
            let record = &mut snapshot.calls[index];
            assert_eq!(record.call_digest, resolve.call_digest, "the digest still matches");
            record.state = match resolve.decision {
                ApprovalDecision::Approved => CallState::Approved,
                ApprovalDecision::Denied => CallState::Denied,
            };
            record.approval = Some((resolve.decision, resolve.api_key_id));
            record.outcome = resolve.outcome.clone();
        }
        Operation::RefuseWaiting(refuse) => {
            let index = call(snapshot, refuse.call_id);
            let record = &mut snapshot.calls[index];
            assert_eq!(record.state, CallState::AwaitingApproval, "only a waiting call");
            assert_eq!(
                record.target.as_ref().and_then(|target| target.installation_id),
                Some(refuse.installation_id),
                "bound to the installation that went"
            );
            record.state = CallState::Refused;
            record.outcome = Some(refuse.outcome.clone());
            record.failure = Some(refuse.failure.clone());
        }
        Operation::AuthorizeCall(authorize) => {
            let index = call(snapshot, authorize.call_id);
            let epoch = snapshot.control.owner_epoch;
            let record = &mut snapshot.calls[index];
            assert!(record.is_open());
            record.decision = Some(authorize.decision.clone());
            match &authorize.authorization {
                Authorization::Send {
                    attempt,
                    dispatch_epoch,
                    invocation_id,
                    send_lease_ms,
                } => {
                    assert_eq!(*dispatch_epoch, epoch);
                    record.state = CallState::Started;
                    record.attempts.push(CallAttempt {
                        attempt: *attempt,
                        dispatch_epoch: *dispatch_epoch,
                        invocation_id: *invocation_id,
                        send_lease_until: after(*send_lease_ms),
                        outcome_recorded: false,
                    });
                }
                Authorization::Refuse { outcome, failure } => {
                    record.state = CallState::Refused;
                    record.outcome = Some(outcome.clone());
                    record.failure = Some(failure.clone());
                }
            }
        }
        Operation::RecordOutcome(record_op) => {
            let index = call(snapshot, record_op.call_id);
            let record = &mut snapshot.calls[index];
            for attempt in &mut record.attempts {
                if attempt.attempt == record_op.attempt {
                    assert!(!attempt.outcome_recorded, "recorded once");
                    attempt.outcome_recorded = true;
                }
            }
            if let Some(settle) = &record_op.settle {
                assert_eq!(record.state, CallState::Started);
                record.state = settle.state;
                record.outcome = Some(settle.outcome.clone());
                record.failure = settle.failure.clone();
            }
        }
        Operation::SettleUnknown(settle) => {
            let index = call(snapshot, settle.call_id);
            let record = &mut snapshot.calls[index];
            assert_eq!(record.state, CallState::Started);
            record.state = CallState::Unknown;
            record.outcome = Some(settle.outcome.clone());
            record.failure = Some(settle.failure.clone());
        }
        Operation::Cancel(cancel) => {
            abandon(snapshot, cancel.abandons);
            close(snapshot, &cancel.closing);
            snapshot.control.cancel_requested_at = Some(now);
        }
        Operation::Yield(Yield {
            why:
                YieldReason::ModelUnavailable {
                    turn,
                    attempt,
                    timed_out,
                    ..
                },
        }) => {
            for recorded in &mut snapshot.model_attempts {
                if (recorded.turn, recorded.attempt) == (*turn, *attempt) {
                    assert_eq!(recorded.state, ModelAttemptState::Prepared);
                    recorded.state = ModelAttemptState::Unanswered;
                    recorded.timed_out = *timed_out;
                }
            }
        }
        Operation::Yield(_) => {}
        Operation::Finish(finish) => {
            abandon(snapshot, finish.abandons);
            if let Some(closing) = &finish.closing {
                close(snapshot, closing);
            }
        }
        Operation::FailAbandoned(failed) => {
            abandon(snapshot, failed.abandons);
            if let Some(closing) = &failed.closing {
                close(snapshot, closing);
            }
        }
    }
    let control = &mut snapshot.control;
    control.status = operation.status_after(control.status);
    control.continuation = operation.continuation_after(control.continuation);
    control.revision += 1;
    if let Some(owner) = guards.owner {
        control.lease_until = owner.renew_lease_ms.map(after);
    }
    if control.status.is_terminal() {
        control.lease_until = None;
    }
}

fn abandon(snapshot: &mut Snapshot, abandons: Option<ModelAttemptRef>) {
    let Some(abandoned) = abandons else { return };
    for attempt in &mut snapshot.model_attempts {
        if (attempt.turn, attempt.attempt) == (abandoned.turn, abandoned.attempt) {
            assert_eq!(attempt.state, ModelAttemptState::Prepared);
            attempt.state = ModelAttemptState::Abandoned;
        }
    }
}

/// Plans and commits one step; what it planned.
fn step(snapshot: &mut Snapshot, facts: &Facts) -> Plan {
    let plan = planned(snapshot, facts);
    let request = plan.request().expect("a transition").clone();
    commit(snapshot, &request, facts.now);
    // Whatever was committed, the shape holds.
    check_shape(snapshot).expect("a consistent run");
    plan
}

#[test]
fn a_run_with_an_approval_and_a_lost_outcome_completes_without_resending() {
    let mut run = fresh();
    let mut now = at(0);
    // Worker `who` holding epoch `epoch`, or about to claim.
    let worker = |who: u128, epoch: Option<u64>, now: Timestamp| Facts {
        claimant: Some(claimant(who)),
        claim: epoch,
        ..Facts { now, ..facts() }
    };

    // Claimed and started.
    assert!(matches!(
        operation(&step(&mut run, &worker(1, None, now))),
        Operation::Claim(_)
    ));
    assert_eq!(run.control.status, RunStatus::Running);
    // The model is asked.
    let asked = step(&mut run, &worker(1, Some(1), now));
    assert!(matches!(asked.io(), Io::CallModel(_)));
    // It asks for a read on Work and a write on Personal.
    let reply = asks(&[("a", WORK_LIST), ("b", PERSONAL_CREATE)]);
    step(
        &mut run,
        &Facts {
            model: Some(answered(0, 1, reply)),
            decisions: vec![(0, allow()), (1, approve())],
            call_ids: vec![call_id(1), call_id(2)],
            ..worker(1, Some(1), now)
        },
    );
    assert_eq!(
        run.control.continuation,
        Continuation::NeedCall { turn: 0, position: 0 }
    );
    // The read goes to Work only, and comes back.
    let read = run.calls[0].clone();
    let sent_read = step(
        &mut run,
        &Facts {
            send_check: Some(check(&read, allow())),
            invocation_id: Some(invocation(1)),
            ..worker(1, Some(1), now)
        },
    );
    let Io::SendCall(send) = sent_read.io() else {
        panic!("sends");
    };
    assert_eq!((send.installation_id, send.tool.as_str()), (work(), "list_tasks"));
    step(
        &mut run,
        &Facts {
            sent: Some(SendResult {
                call_id: read.id,
                attempt: 1,
                invocation_id: invocation(1),
                outcome: SendOutcome::Completed { result: result() },
            }),
            ..worker(1, Some(1), now)
        },
    );
    assert_eq!(run.calls[0].state, CallState::Completed);
    // The write waits for a person, its installation still there; the
    // claim is released.
    let waiting = run.calls[1].clone();
    step(
        &mut run,
        &Facts {
            send_check: Some(check(&waiting, approve())),
            ..worker(1, Some(1), now)
        },
    );
    assert_eq!(run.control.status, RunStatus::WaitingForApproval);
    assert_eq!(run.control.lease_until, None);
    assert_eq!(
        planned(&run, &worker(1, Some(1), now)),
        Plan::Idle(Idle::AwaitingApproval)
    );
    // An API key approves it.
    let approval = decide(&run, call_id(2), ApprovalDecision::Approved, None).expect("resolves");
    commit(&mut run, &approval, now);
    // A new claim sends it to Personal, under the approval.
    assert!(matches!(
        operation(&step(&mut run, &worker(2, None, now))),
        Operation::Claim(Claim { epoch: 2, .. })
    ));
    let write = run.calls[1].clone();
    let sent_write = step(
        &mut run,
        &Facts {
            send_check: Some(check(&write, approve())),
            invocation_id: Some(invocation(2)),
            ..worker(2, Some(2), now)
        },
    );
    let Io::SendCall(send) = sent_write.io() else {
        panic!("sends");
    };
    assert_eq!((send.installation_id, send.tool.as_str()), (personal(), "create_task"));
    // The worker dies mid-send. Until the send lease lapses nobody touches
    // the call.
    now = at(Limits::default().send_lease_ms as i64 - 1);
    let busy = planned(&run, &worker(3, None, now));
    assert!(matches!(busy, Plan::Idle(Idle::Busy { .. })));
    // Then a new worker claims the run and settles the call unknown.
    now = at(Limits::default().lease_ms as i64 + 1);
    assert!(matches!(
        operation(&step(&mut run, &worker(3, None, now))),
        Operation::Claim(Claim { epoch: 3, .. })
    ));
    assert!(matches!(
        operation(&step(&mut run, &worker(3, Some(3), now))),
        Operation::SettleUnknown(_)
    ));
    assert_eq!(run.calls[1].state, CallState::Unknown);
    // The old sender's answer arrives: kept as evidence, the call stays
    // unknown, and the old worker can write nothing else.
    let late = step(
        &mut run,
        &Facts {
            sent: Some(SendResult {
                call_id: write.id,
                attempt: 1,
                invocation_id: invocation(2),
                outcome: SendOutcome::Completed { result: result() },
            }),
            ..worker(2, Some(2), now)
        },
    );
    assert!(matches!(
        operation(&late),
        Operation::RecordOutcome(RecordOutcome { settle: None, .. })
    ));
    assert_eq!(run.calls[1].state, CallState::Unknown);
    assert_eq!(planned(&run, &worker(2, Some(2), now)), Plan::Idle(Idle::Lost));
    // The model reads what happened and answers.
    let asked = step(&mut run, &worker(3, Some(3), now));
    let Io::CallModel(request) = asked.io() else {
        panic!("asks the model");
    };
    assert!(request.messages.contains(&Message::ToolResult {
        call_id: "b".to_owned(),
        content: UNKNOWN.to_owned(),
        is_error: true,
    }));
    step(
        &mut run,
        &Facts {
            model: Some(answered(1, 1, Reply::Text("Listed, and a task may exist.".to_owned()))),
            ..worker(3, Some(3), now)
        },
    );
    assert_eq!(run.control.status, RunStatus::Completed);
    assert_eq!(planned(&run, &worker(3, Some(3), now)), Plan::Idle(Idle::Finished));
    // Each call was authorized to send at most once.
    assert!(run.calls.iter().all(|call| call.attempts.len() <= 1));
    assert!(run.calls.iter().all(|call| call.state.is_settled()));
    // Fourteen transitions after admission, each raising the revision once.
    assert_eq!(run.control.revision, 15);
}

#[test]
fn a_worker_that_dies_before_the_answer_is_committed_asks_again() {
    let mut run = fresh();
    let now = at(0);
    step(&mut run, &facts());
    step(&mut run, &claimed());
    assert_eq!(
        run.control.continuation,
        Continuation::AwaitModel { turn: 0, attempt: 1 }
    );
    // A new worker after the lease lapses.
    let later = at(Limits::default().lease_ms as i64 + 1);
    let take = |claim| Facts {
        claimant: Some(claimant(2)),
        claim,
        now: later,
        ..facts()
    };
    step(&mut run, &take(None));
    let retried = step(&mut run, &take(Some(2)));
    assert!(matches!(
        operation(&retried),
        Operation::PrepareModel(PrepareModel {
            attempt: 2,
            abandons: Some(1),
            ..
        })
    ));
    let states: Vec<ModelAttemptState> = run.model_attempts.iter().map(|attempt| attempt.state).collect();
    assert_eq!(states, [ModelAttemptState::Abandoned, ModelAttemptState::Prepared]);
    // The committed answer is the second attempt's; the first is never
    // asked for again.
    step(
        &mut run,
        &Facts {
            model: Some(answered(0, 2, Reply::Text("Done.".to_owned()))),
            ..take(Some(2))
        },
    );
    assert_eq!(run.control.status, RunStatus::Completed);
    assert_eq!(run.turns.len(), 1);
    assert!(now < later);
}

#[test]
fn a_cancel_after_authorization_records_the_send_and_runs_nothing_after() {
    let mut run = sending(30_000);
    let cancelled = cancel(
        organization(),
        workspace(),
        &run,
        caller(1),
        RunTrigger::Api,
        canceller(),
    )
    .expect("cancels");
    commit(&mut run, &cancelled, at(0));
    assert_eq!(run.control.status, RunStatus::Cancelled);
    assert_eq!(run.calls[1].state, CallState::Refused, "the call not sent is closed");
    assert_eq!(run.calls[0].state, CallState::Started, "the one sent isn't");
    // No worker step after it: only a wait for the send in flight.
    assert_eq!(planned(&run, &claimed()), Plan::Idle(Idle::Busy { until: at(30_000) }));
    // The sender's outcome is still recorded; the run stays cancelled.
    step(&mut run, &sent(SendOutcome::Completed { result: result() }));
    assert_eq!(run.calls[0].state, CallState::Completed);
    assert_eq!(run.control.status, RunStatus::Cancelled);
    assert_eq!(run.control.continuation, Continuation::Done);
    assert!(run.calls.iter().all(|call| !call.is_open()));
}

#[test]
fn a_definition_changed_while_waiting_is_refused_at_send() {
    let mut run = parked();
    let approval = decide(&run, call_id(2), ApprovalDecision::Approved, None).expect("resolves");
    commit(&mut run, &approval, at(0));
    step(&mut run, &facts());
    let waiting = run.calls[1].clone();
    let changed = SendCheck {
        definition_sha256: Some(Sha256Digest::of(b"create_task, changed")),
        ..check(&waiting, approve())
    };
    let refused_plan = step(
        &mut run,
        &Facts {
            claim: Some(2),
            send_check: Some(changed),
            invocation_id: Some(invocation(1)),
            ..facts()
        },
    );
    assert_eq!(refused_plan.io(), &Io::Nothing);
    assert_eq!(run.calls[1].state, CallState::Refused);
    assert_eq!(
        run.calls[1].failure.as_ref().map(|f| f.code.as_str()),
        Some("tool_changed")
    );
    assert_eq!(
        run.control.continuation,
        Continuation::NeedCall { turn: 0, position: 2 }
    );
}

// ---------------------------------------------------------------------------
// Claimants, callers, sends on finished runs, scope, abandoned attempts

#[test]
fn two_claimants_from_the_same_revision_conflict_and_only_a_retry_replays() {
    let snapshot = fresh();
    let by = |who: u128| Facts {
        claimant: Some(claimant(who)),
        ..facts()
    };
    let a = planned(&snapshot, &by(1));
    let b = planned(&snapshot, &by(2));
    let (a, b) = (a.request().expect("claims"), b.request().expect("claims"));
    assert_eq!(a.id(), b.id(), "at most one commits");
    assert_ne!(a.digest(), b.digest(), "the other conflicts, never replays");
    assert_eq!(
        planned(&snapshot, &by(1)).request(),
        Some(a),
        "a retry is the same request"
    );
    let anonymous = Facts {
        claimant: None,
        ..facts()
    };
    assert_eq!(
        advance(&snapshot, &anonymous),
        Err(AdvanceError::MissingFact("claimant"))
    );
}

#[test]
fn a_step_is_its_claimants_own() {
    // Claimant 1 holds epoch 1. Claimant 2, holding nothing but naming the
    // same epoch, has lost: its step would never match the owner guard.
    let snapshot = running(Continuation::NeedModel { turn: 0 });
    let other = Facts {
        claimant: Some(claimant(2)),
        ..claimed()
    };
    assert_eq!(planned(&snapshot, &other), Plan::Idle(Idle::Lost));
    let mine = planned(&snapshot, &claimed());
    assert_eq!(guards(&mine).owner.map(|owner| owner.claimant), Some(claimant(1)));
    // Even built by hand, claimant 2's PrepareModel isn't claimant 1's.
    let request = mine.request().expect("prepares");
    let mut forged = *request.guards();
    if let Some(owner) = forged.owner.as_mut() {
        owner.claimant = claimant(2);
    }
    let theirs = TransitionRequest::new(
        organization(),
        workspace(),
        run_id(),
        request.from_revision(),
        forged,
        request.operation().clone(),
    );
    assert_eq!(theirs.id(), request.id());
    assert_ne!(theirs.digest(), request.digest());
    let missing = Facts {
        claimant: None,
        ..claimed()
    };
    assert_eq!(advance(&snapshot, &missing), Err(AdvanceError::MissingFact("claimant")));
}

#[test]
fn a_dead_senders_call_on_a_cancelled_run_is_settled_after_its_lease() {
    let mut run = sending(30_000);
    let cancelled = cancel(
        organization(),
        workspace(),
        &run,
        caller(1),
        RunTrigger::Api,
        canceller(),
    )
    .expect("cancels");
    commit(&mut run, &cancelled, at(0));
    // Its sender never answers. Until the send lease lapses, wait.
    assert_eq!(planned(&run, &facts()), Plan::Idle(Idle::Busy { until: at(30_000) }));
    let later = Facts {
        now: at(30_000),
        ..facts()
    };
    let plan = step(&mut run, &later);
    let Operation::SettleUnknown(settle) = operation(&plan) else {
        panic!("settles unknown");
    };
    assert_eq!(settle.next, None, "the run stays where it is");
    assert_eq!(
        guards(&plan),
        Guards {
            send: Some(SendGuard {
                call_id: call_id(1),
                attempt: 1,
                dispatch_epoch: 1,
                invocation_id: invocation(1),
                lease: SendLease::Lapsed,
            }),
            ..Guards::default()
        },
        "fenced by the send claim alone"
    );
    assert_eq!(run.calls[0].state, CallState::Unknown);
    assert_eq!(run.control.status, RunStatus::Cancelled);
    assert_eq!(run.control.continuation, Continuation::Done);
    assert_eq!(planned(&run, &later), Plan::Idle(Idle::Finished));
    assert!(run.calls.iter().all(|call| call.state.is_settled()));
}

#[test]
fn the_sweep_waits_for_a_send_in_flight_then_its_call_is_settled() {
    let mut run = sending(30_000);
    run.control.lease_until = Some(at(-1));
    assert_eq!(
        fail_abandoned(organization(), workspace(), &run, caller(1), at(0)),
        Err(ControlError::Sending)
    );
    let failed = fail_abandoned(organization(), workspace(), &run, caller(1), at(30_000)).expect("fails it");
    let Operation::FailAbandoned(failed_op) = failed.operation() else {
        panic!("fails");
    };
    assert_eq!(
        failed_op.closing.as_ref().map(|closing| closing.calls.clone()),
        Some(vec![call_id(2)]),
        "the call being sent isn't closed"
    );
    commit(&mut run, &failed, at(30_000));
    assert_eq!(run.control.status, RunStatus::Failed);
    assert_eq!(run.calls[0].state, CallState::Started);
    let later = Facts {
        now: at(30_000),
        ..facts()
    };
    assert!(matches!(
        operation(&step(&mut run, &later)),
        Operation::SettleUnknown(SettleUnknown { next: None, .. })
    ));
    assert!(run.calls.iter().all(|call| call.state.is_settled()));
}

#[test]
fn control_operations_refuse_a_run_of_another_scope() {
    let other_organization = OrganizationId::from_uuid(Uuid::from_u128(91));
    let other_workspace = WorkspaceId::from_uuid(Uuid::from_u128(92));
    for (organization_id, workspace_id) in [
        (other_organization, workspace()),
        (organization(), other_workspace),
        (other_organization, other_workspace),
    ] {
        assert_eq!(
            resolve_approval(
                organization_id,
                workspace_id,
                &parked(),
                caller(1),
                approval(call_id(2), ApprovalDecision::Approved, None)
            ),
            Err(ControlError::NotFound)
        );
        assert_eq!(
            cancel(
                organization_id,
                workspace_id,
                &parked(),
                caller(1),
                RunTrigger::Api,
                canceller()
            ),
            Err(ControlError::NotFound)
        );
        assert_eq!(
            fail_abandoned(organization_id, workspace_id, &fresh(), caller(1), at(0)),
            Err(ControlError::NotFound)
        );
    }
}

#[test]
fn a_run_ended_while_awaiting_the_model_abandons_the_attempt() {
    let mut run = awaiting_model();
    let failed = Facts {
        model: Some(ModelAnswer {
            turn: 0,
            attempt: 1,
            result: ModelResult::Failed(Failure::new("model_failed", "No.")),
        }),
        ..claimed()
    };
    let plan = step(&mut run, &failed);
    let Operation::Finish(finish) = operation(&plan) else {
        panic!("finishes");
    };
    assert_eq!(finish.abandons, Some(ModelAttemptRef { turn: 0, attempt: 1 }));
    assert_eq!(run.model_attempts[0].state, ModelAttemptState::Abandoned);
    // Cancelled or swept there, the same.
    let cancelled = cancel(
        organization(),
        workspace(),
        &awaiting_model(),
        caller(1),
        RunTrigger::Api,
        canceller(),
    )
    .expect("cancels");
    assert!(matches!(
        cancelled.operation(),
        Operation::Cancel(Cancel {
            abandons: Some(ModelAttemptRef { turn: 0, attempt: 1 }),
            ..
        })
    ));
    let mut lapsed = awaiting_model();
    lapsed.control.lease_until = Some(at(-1));
    let swept = fail_abandoned(organization(), workspace(), &lapsed, caller(1), at(0)).expect("fails it");
    assert!(matches!(
        swept.operation(),
        Operation::FailAbandoned(FailAbandoned {
            abandons: Some(ModelAttemptRef { turn: 0, attempt: 1 }),
            ..
        })
    ));
}

#[test]
fn two_callers_of_one_control_operation_conflict() {
    let snapshot = parked();
    let first = decide(&snapshot, call_id(2), ApprovalDecision::Approved, None).expect("resolves");
    let second = resolve_approval(
        organization(),
        workspace(),
        &snapshot,
        caller(2),
        approval(call_id(2), ApprovalDecision::Approved, None),
    )
    .expect("resolves");
    // One revision, so at most one commits; the other conflicts and the
    // caller answers 409 as today, never replaying the first's receipt.
    assert_eq!(first.id(), second.id());
    assert_ne!(first.digest(), second.digest());
    assert_eq!(
        decide(&snapshot, call_id(2), ApprovalDecision::Approved, None).expect("resolves"),
        first,
        "the same caller's retry replays"
    );
    let cancel_by = |n| {
        cancel(
            organization(),
            workspace(),
            &snapshot,
            caller(n),
            RunTrigger::Api,
            canceller(),
        )
        .expect("cancels")
    };
    assert_eq!(cancel_by(1), cancel_by(1));
    assert_ne!(cancel_by(1).digest(), cancel_by(2).digest());
    let mut lapsed = running(Continuation::NeedModel { turn: 0 });
    lapsed.control.lease_until = None;
    let sweep_by = |n| fail_abandoned(organization(), workspace(), &lapsed, caller(n), at(0)).expect("fails it");
    assert_ne!(sweep_by(1).digest(), sweep_by(2).digest());
}

/// Floats a model may write, with 17 significant digits.
const LONG_FLOAT: &str = "99982.865181082772";

/// `value` as the store would hand it back: written as text, read again.
fn stored(value: &Value) -> Value {
    serde_json::from_str(&value.to_string()).expect("reads back")
}

#[test]
fn long_floats_in_arguments_survive_storage() {
    let mut call = record(1, 0, WORK_CREATE, CallState::Allowed);
    let arguments: Value = serde_json::from_str(&format!(r#"{{"amount":{LONG_FLOAT},"title":"Q3"}}"#)).expect("parses");
    let Value::Object(arguments) = arguments else {
        panic!("an object");
    };
    let target = call.target.clone().expect("a target");
    call.call_digest = Some(call_digest(
        target.installation_id.expect("an app tool"),
        &target.app_key,
        &target.tool,
        &target.definition_sha256,
        &arguments,
    ));
    // The store keeps the arguments as text and reads them back.
    let Value::Object(read_back) = stored(&Value::Object(arguments)) else {
        panic!("an object");
    };
    call.arguments = read_back;
    let snapshot = with_calls(Continuation::NeedCall { turn: 0, position: 0 }, vec![call.clone()]);
    let facts = Facts {
        send_check: Some(check(&call, allow())),
        invocation_id: Some(invocation(1)),
        ..claimed()
    };
    let plan = planned(&snapshot, &facts);
    assert!(
        matches!(
            operation(&plan),
            Operation::AuthorizeCall(AuthorizeCall {
                authorization: Authorization::Send { .. },
                ..
            })
        ),
        "an unchanged call isn't refused as changed"
    );
}

#[test]
fn long_floats_in_a_frozen_offer_survive_storage() {
    let schema: Value = serde_json::from_str(&format!(
        r#"{{"type":"object","properties":{{"amount":{{"type":"number","maximum":{LONG_FLOAT}}}}}}}"#
    ))
    .expect("parses");
    let mut long = candidate(work(), "k7m2qd", "Work", "pay");
    long.input_schema = schema;
    let frozen = build_offer([long]).offer;
    let mut snapshot = running(Continuation::AwaitModel { turn: 0, attempt: 1 });
    let digest = model_request(&snapshot, 0, 1, None, (&frozen, None), None)
        .expect("a request")
        .digest();
    // The store keeps the offer as JSON and reads it back.
    let read_back: Offer =
        serde_json::from_value(stored(&serde_json::to_value(&frozen).expect("writes"))).expect("reads back");
    snapshot.model_attempts.push(ModelAttempt {
        turn: 0,
        attempt: 1,
        state: ModelAttemptState::Prepared,
        request_digest: digest,
        offer: read_back,
        memory: None,
        timed_out: false,
    });
    let facts = Facts {
        model: Some(answered(0, 1, Reply::Text("Paid.".to_owned()))),
        ..claimed()
    };
    assert!(
        matches!(operation(&planned(&snapshot, &facts)), Operation::CommitTurn(_)),
        "the request's digest still matches"
    );
}

/// A channel run in a session: two earlier messages of history, the
/// session's own offer, and no tools of its own read.
fn in_session() -> Snapshot {
    let mut snapshot = running(Continuation::NeedModel { turn: 0 });
    snapshot.input = "[Ana] and now?".to_owned();
    snapshot.session = Some(crate::runs::state::SessionContext {
        origin: origin(),
        history: vec![
            Message::User {
                text: "[Ana] hello".to_owned(),
            },
            Message::Assistant {
                text: Some("Hi.".to_owned()),
                tool_calls: Vec::new(),
            },
        ],
        offer: offer(),
        steers: Vec::new(),
    });
    snapshot
}

fn steer(text: &str) -> crate::runs::transition::Steer {
    crate::runs::transition::Steer {
        text: text.to_owned(),
        dispatches: vec![crate::channels::store::ChannelDispatchKey::from_uuid(Uuid::from_u128(
            70,
        ))],
        message_ids: vec![crate::ids::ChannelMessageId::from_uuid(Uuid::from_u128(71))],
        seen: crate::runs::transition::SeenMessage {
            id: crate::ids::ChannelMessageId::from_uuid(Uuid::from_u128(71)),
            at: at(0),
        },
        changes_noted_at: None,
    }
}

fn user(text: &str) -> Message {
    Message::User { text: text.to_owned() }
}

#[test]
fn a_channel_run_continues_its_session_with_the_sessions_tools() {
    let snapshot = in_session();
    // The session's tools: none read for the run.
    let facts = Facts {
        offer: None,
        ..claimed()
    };
    let plan = planned(&snapshot, &facts);
    let Io::CallModel(request) = plan.io() else {
        panic!("calls the model");
    };
    assert_eq!(
        request.messages[..2],
        in_session().session.expect("a session").history[..]
    );
    assert_eq!(request.messages[2], user("[Ana] and now?"));
    assert_eq!(request.tools.len(), 3);
}

#[test]
fn a_steer_joins_only_a_turns_first_attempt_and_is_resent_as_stored() {
    let mut snapshot = in_session();
    snapshot.turns.push(CommittedTurn {
        turn: 0,
        text: Some("Looking.".to_owned()),
    });
    snapshot.calls.push(CallRecord {
        outcome: Some(CallOutcome {
            content: "{}".to_owned(),
            is_error: false,
        }),
        ..record(1, 0, WORK_LIST, CallState::Completed)
    });
    snapshot.control.continuation = Continuation::NeedModel { turn: 1 };
    let joined = steer("New in the thread while you worked:\n[Bo] also this");
    let facts = Facts {
        steer: Some(joined.clone()),
        ..claimed()
    };
    let plan = planned(&snapshot, &facts);
    let Io::CallModel(request) = plan.io() else {
        panic!("calls the model");
    };
    let Operation::PrepareModel(prepared) = operation(&plan) else {
        panic!("prepares");
    };
    assert_eq!(prepared.steer.as_ref(), Some(&joined));
    assert_eq!(request.messages.len(), 6, "{:?}", request.messages);
    assert!(matches!(request.messages[4], Message::ToolResult { .. }));
    assert_eq!(request.messages[5], user(&joined.text));

    // Stored, it sits where it was sent, and a retry sends the same bytes
    // without taking anything new in.
    let mut stored = snapshot.clone();
    if let Some(session) = stored.session.as_mut() {
        session.steers.push(crate::runs::state::SteeredTurn {
            turn: 1,
            text: joined.text.clone(),
        });
    }
    stored.model_attempts.push(ModelAttempt {
        turn: 1,
        attempt: 1,
        state: ModelAttemptState::Unanswered,
        request_digest: request.digest(),
        offer: offer(),
        memory: None,
        timed_out: false,
    });
    let retry = planned(
        &stored,
        &Facts {
            steer: Some(steer("another")),
            ..claimed()
        },
    );
    let Io::CallModel(again) = retry.io() else {
        panic!("calls the model again");
    };
    let Operation::PrepareModel(prepared) = operation(&retry) else {
        panic!("prepares");
    };
    assert_eq!(prepared.attempt, 2);
    assert_eq!(prepared.steer, None);
    assert_eq!(again.messages, request.messages);
    assert_eq!(again.digest(), request.digest());
}

#[test]
fn a_run_outside_a_session_takes_no_steer() {
    let snapshot = running(Continuation::NeedModel { turn: 0 });
    let plan = planned(
        &snapshot,
        &Facts {
            steer: Some(steer("ignored")),
            ..claimed()
        },
    );
    let Operation::PrepareModel(prepared) = operation(&plan) else {
        panic!("prepares");
    };
    assert_eq!(prepared.steer, None);
}

// ---------------------------------------------------------------------------
// A channel session's own tools

/// The session's channel tools: `reply` through its origin's connection,
/// and `stay_silent`.
fn channel_offer() -> Offer {
    let connection = crate::channels::tools::OfferingConnection {
        id: origin().connection_id,
        provider: crate::channels::ChannelProviderKind::Slack,
        short_id: "c4n7xt",
        label: "helper",
        handle: "helper",
    };
    Offer::new(vec![
        crate::channels::tools::offered_tool(&connection, "reply").expect("offered"),
        crate::channels::tools::stay_silent().expect("offered"),
    ])
    .expect("an offer")
}

const CHANNEL_REPLY: &str = "slack_c4n7xt_reply";

/// A channel run whose session offers its channel tools, turn 0 prepared
/// as attempt 1 and awaiting its answer.
fn channel_awaiting_model() -> Snapshot {
    let mut snapshot = in_session();
    if let Some(session) = snapshot.session.as_mut() {
        session.offer = channel_offer();
    }
    snapshot.control.continuation = Continuation::AwaitModel { turn: 0, attempt: 1 };
    let digest = model_request(&snapshot, 0, 1, None, (&channel_offer(), None), None)
        .expect("a request")
        .digest();
    snapshot.model_attempts.push(ModelAttempt {
        turn: 0,
        attempt: 1,
        state: ModelAttemptState::Prepared,
        request_digest: digest,
        offer: channel_offer(),
        memory: None,
        timed_out: false,
    });
    snapshot
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(object) => object,
        other => panic!("not an object: {other}"),
    }
}

fn calling(calls: &[(&str, &str, Value)]) -> Reply {
    Reply::ToolCalls {
        text: None,
        calls: calls
            .iter()
            .map(|(id, name, arguments)| ModelToolCall {
                id: (*id).to_owned(),
                name: (*name).to_owned(),
                arguments: object(arguments.clone()),
            })
            .collect(),
    }
}

fn committed(plan: &Plan) -> &CommitTurn {
    match operation(plan) {
        Operation::CommitTurn(commit) => commit,
        other => panic!("not a committed turn: {other:?}"),
    }
}

#[test]
fn stay_silent_ends_a_channel_run_with_its_reason_once_its_calls_are_settled() {
    let silent = calling(&[(
        "s",
        crate::channels::tools::STAY_SILENT,
        json!({ "reason": " Not for me. " }),
    )]);
    let facts = Facts {
        model: Some(answered(0, 1, silent)),
        call_ids: vec![call_id(1)],
        offer: None,
        ..claimed()
    };
    let plan = planned(&channel_awaiting_model(), &facts);
    let commit = committed(&plan);
    assert_eq!(commit.silent.as_deref(), Some("Not for me."));
    assert_eq!(commit.next, Continuation::Done);
    assert_eq!(commit.ending, Some(RunEnding::Completed { output: String::new() }));
    // The run's own: settled as recorded, bound to nothing, sent nowhere.
    let call = &commit.calls[0];
    assert_eq!(call.state, CallState::Completed);
    assert_eq!((call.target.as_ref(), call.call_digest), (None, None));
    assert_eq!(
        call.outcome.as_ref().map(|outcome| outcome.content.as_str()),
        Some(crate::channels::tools::STAYED_SILENT)
    );
    assert_eq!(guards(&plan), releases(1));

    // Beside a reply asked for first, the reply is sent, bound to the run's
    // own connection and where it began, and the silence refused: a run
    // either posts or stays silent.
    let both = calling(&[
        ("r", CHANNEL_REPLY, json!({ "text": "Done." })),
        (
            "s",
            crate::channels::tools::STAY_SILENT,
            json!({ "reason": "Answered above." }),
        ),
    ]);
    let facts = Facts {
        model: Some(answered(0, 1, both)),
        call_ids: vec![call_id(1), call_id(2)],
        decisions: vec![(0, allow())],
        offer: None,
        ..claimed()
    };
    let plan = planned(&channel_awaiting_model(), &facts);
    let commit = committed(&plan);
    assert_eq!(commit.silent, None);
    assert_eq!(commit.next, Continuation::NeedCall { turn: 0, position: 0 });
    assert_eq!(commit.ending, None);
    assert_eq!(commit.calls[1].state, CallState::Refused);
    assert_eq!(
        commit.calls[1].outcome.as_ref().map(|outcome| outcome.content.as_str()),
        Some(POSTED_SO_NOT_SILENT)
    );
    let reply = &commit.calls[0];
    let target = reply.target.as_ref().expect("bound");
    assert_eq!(target.channel(), Some(origin().connection_id));
    assert_eq!(target.installation_id, None);
    assert_eq!(
        reply.call_digest,
        Some(crate::runs::digest::channel_call_digest(
            origin().connection_id,
            "slack",
            "reply",
            &crate::channels::tools::definition_sha256("reply").expect("defined"),
            &object(json!({ "text": "Done." })),
            (origin().conversation_id, origin().thread_key.as_str()),
        ))
    );

    // Without a reason, it isn't silence: the model is told to give one.
    let bare = calling(&[("s", crate::channels::tools::STAY_SILENT, json!({ "reason": "  " }))]);
    let facts = Facts {
        model: Some(answered(0, 1, bare)),
        call_ids: vec![call_id(1)],
        offer: None,
        ..claimed()
    };
    let plan = planned(&channel_awaiting_model(), &facts);
    let commit = committed(&plan);
    assert_eq!(commit.silent, None);
    assert_eq!(commit.next, Continuation::NeedModel { turn: 1 });
    assert_eq!(commit.calls[0].state, CallState::Refused);
    assert_eq!(
        commit.calls[0].outcome.as_ref().map(|outcome| outcome.content.as_str()),
        Some(crate::channels::tools::SILENT_WITHOUT_REASON)
    );
}

#[test]
fn an_empty_channel_answer_is_nudged_once_then_fails_the_run() {
    let empty = |turn: u32| Facts {
        model: Some(answered(turn, 1, Reply::Text("  ".to_owned()))),
        offer: None,
        ..claimed()
    };
    let plan = planned(&channel_awaiting_model(), &empty(0));
    let commit = committed(&plan);
    assert_eq!(
        (commit.next, commit.ending.clone()),
        (Continuation::NeedModel { turn: 1 }, None)
    );
    assert_eq!(guards(&plan), keeps(1));

    // The next turn says so in place of the empty answer: no fallback text.
    let mut nudged = channel_awaiting_model();
    nudged.model_attempts[0].state = ModelAttemptState::Answered;
    nudged.turns.push(CommittedTurn {
        turn: 0,
        text: Some("  ".to_owned()),
    });
    nudged.control.continuation = Continuation::NeedModel { turn: 1 };
    let plan = planned(
        &nudged,
        &Facts {
            offer: None,
            ..claimed()
        },
    );
    let Io::CallModel(request) = plan.io() else {
        panic!("calls the model");
    };
    assert_eq!(request.messages.last(), Some(&user(crate::runs::history::NUDGE)));

    // A second empty answer fails it.
    nudged.model_attempts.push(ModelAttempt {
        turn: 1,
        attempt: 1,
        state: ModelAttemptState::Prepared,
        request_digest: request.digest(),
        offer: channel_offer(),
        memory: None,
        timed_out: false,
    });
    nudged.control.continuation = Continuation::AwaitModel { turn: 1, attempt: 1 };
    let plan = planned(&nudged, &empty(1));
    let commit = committed(&plan);
    let Some(RunEnding::Failed { error }) = &commit.ending else {
        panic!("fails: {commit:?}");
    };
    assert_eq!(error.code, EMPTY_OUTPUT);
    assert_eq!(commit.next, Continuation::Done);

    // Outside a channel session, an empty answer is the run's output.
    let plan = planned(
        &awaiting_model(),
        &Facts {
            model: Some(answered(0, 1, Reply::Text(String::new()))),
            ..claimed()
        },
    );
    assert_eq!(
        committed(&plan).ending,
        Some(RunEnding::Completed { output: String::new() })
    );
}

/// A channel run whose turn 0 called its reply tool, the call settled
/// `state`, and whose turn 1 awaits its answer.
fn channel_after_reply(state: CallState) -> Snapshot {
    let answer = calling(&[("r", CHANNEL_REPLY, json!({ "text": "Done." }))]);
    let facts = Facts {
        model: Some(answered(0, 1, answer)),
        call_ids: vec![call_id(1)],
        decisions: vec![(0, allow())],
        offer: None,
        ..claimed()
    };
    let plan = planned(&channel_awaiting_model(), &facts);
    let new = committed(&plan).calls[0].clone();
    let mut snapshot = channel_awaiting_model();
    snapshot.model_attempts[0].state = ModelAttemptState::Answered;
    snapshot.turns.push(CommittedTurn { turn: 0, text: None });
    snapshot.calls.push(CallRecord {
        id: new.id,
        turn: 0,
        position: new.position,
        model_call_id: new.model_call_id,
        model_tool_name: new.model_tool_name,
        target: new.target,
        arguments: new.arguments,
        state,
        decision: new.decision,
        call_digest: new.call_digest,
        approval: None,
        outcome: Some(CallOutcome {
            content: "{\"posted\":true}".to_owned(),
            is_error: state != CallState::Completed,
        }),
        failure: None,
        attempts: Vec::new(),
    });
    snapshot.control.continuation = Continuation::NeedModel { turn: 1 };
    let plan = planned(
        &snapshot,
        &Facts {
            offer: None,
            ..claimed()
        },
    );
    let Io::CallModel(request) = plan.io() else {
        panic!("calls the model");
    };
    snapshot.model_attempts.push(ModelAttempt {
        turn: 1,
        attempt: 1,
        state: ModelAttemptState::Prepared,
        request_digest: request.digest(),
        offer: channel_offer(),
        memory: None,
        timed_out: false,
    });
    snapshot.control.continuation = Continuation::AwaitModel { turn: 1, attempt: 1 };
    snapshot
}

#[test]
fn a_run_that_posted_or_may_have_cant_stay_silent_and_isnt_nudged() {
    for state in [CallState::Completed, CallState::Unknown] {
        // Its blank answer after the post ends it: the thread was answered,
        // or may have been.
        let blank = Facts {
            model: Some(answered(1, 1, Reply::Text("  ".to_owned()))),
            offer: None,
            ..claimed()
        };
        let commit = committed(&planned(&channel_after_reply(state), &blank)).clone();
        assert_eq!(commit.next, Continuation::Done, "{state:?}");
        assert!(
            matches!(commit.ending, Some(RunEnding::Completed { .. })),
            "{state:?}: {:?}",
            commit.ending
        );

        // Silence after it is refused, and the run goes on.
        let silent = Facts {
            model: Some(answered(
                1,
                1,
                calling(&[(
                    "s",
                    crate::channels::tools::STAY_SILENT,
                    json!({ "reason": "Nothing to add." }),
                )]),
            )),
            call_ids: vec![call_id(2)],
            offer: None,
            ..claimed()
        };
        let commit = committed(&planned(&channel_after_reply(state), &silent)).clone();
        assert_eq!(commit.silent, None, "{state:?}");
        assert_eq!(commit.calls[0].state, CallState::Refused);
        assert_eq!(
            commit.calls[0].outcome.as_ref().map(|outcome| outcome.content.as_str()),
            Some(POSTED_SO_NOT_SILENT)
        );
        assert_eq!(commit.next, Continuation::NeedModel { turn: 2 });
    }
}

#[test]
fn a_post_after_stay_silent_in_its_turn_is_refused() {
    let answer = calling(&[
        (
            "s",
            crate::channels::tools::STAY_SILENT,
            json!({ "reason": "Ana has it." }),
        ),
        ("r", CHANNEL_REPLY, json!({ "text": "Done." })),
    ]);
    let facts = Facts {
        model: Some(answered(0, 1, answer)),
        call_ids: vec![call_id(1), call_id(2)],
        decisions: vec![(1, allow())],
        offer: None,
        ..claimed()
    };
    let plan = planned(&channel_awaiting_model(), &facts);
    let commit = committed(&plan);
    assert_eq!(commit.silent.as_deref(), Some("Ana has it."));
    assert_eq!(commit.calls[1].state, CallState::Refused);
    assert_eq!(
        commit.calls[1].failure.as_ref().map(|failure| failure.message.as_str()),
        Some(SILENT_SO_NOT_POSTED)
    );
    assert_eq!(commit.next, Continuation::Done);
    assert_eq!(commit.ending, Some(RunEnding::Completed { output: String::new() }));
}

#[test]
fn a_runs_channel_changes_count_edits_and_reactions_beside_posts() {
    let connection = crate::channels::tools::OfferingConnection {
        id: origin().connection_id,
        provider: crate::channels::ChannelProviderKind::Slack,
        short_id: "c4n7xt",
        label: "helper",
        handle: "helper",
    };
    let call = |n: u128, tool: &str, state: CallState| {
        let offered = crate::channels::tools::offered_tool(&connection, tool).expect("offered");
        CallRecord {
            id: call_id(n),
            turn: 0,
            position: 0,
            model_call_id: format!("m{n}"),
            model_tool_name: offered.name.clone(),
            target: bound(&offered),
            arguments: Map::new(),
            state,
            decision: None,
            call_digest: None,
            approval: None,
            outcome: None,
            failure: None,
            attempts: Vec::new(),
        }
    };
    let mut snapshot = in_session();
    snapshot.calls = vec![
        call(1, "reply", CallState::Completed),
        call(2, "react", CallState::Completed),
        call(3, "edit_message", CallState::Unknown),
        call(4, "delete_message", CallState::AwaitingApproval),
        call(5, "react", CallState::Refused),
        call(6, "send_message", CallState::Failed),
        call(7, "read_messages", CallState::Completed),
    ];
    assert_eq!(channel_changes(&snapshot), 4);
}
