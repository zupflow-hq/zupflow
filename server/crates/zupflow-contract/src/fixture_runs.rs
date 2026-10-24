//! The fixture's runs, taken step by step through the reducer and the run
//! store as a worker takes them (decision 0046), so their timelines, calls
//! and records read like real ones. The model's answers are written here;
//! nothing is sent to an app's server.

use serde_json::{Map, Value, json};
use uuid::Uuid;
use zupflow_core::ids::{AgentId, InstallationId, OrganizationId, RunId, SessionId, ToolCallId, WorkspaceId};
use zupflow_core::model_store::{FIRST_KEY_VERSION, RouteState, RunModel};
use zupflow_core::models::{Completion, ModelToolCall, Reply, TokenUsage};
use zupflow_core::runs::digest::Sha256Digest;
use zupflow_core::runs::names::{Candidate, build_offer};
use zupflow_core::runs::reducer::{
    Facts, InstallationCheck, Io, ModelAnswer, ModelResult, Plan, SendCheck, SendResult, admit, advance,
};
use zupflow_core::runs::state::{CallDecision, CallVerdict, ClaimantId, InvocationId, Snapshot};
use zupflow_core::runs::store::{Applied, RunStore};
use zupflow_core::runs::transition::SendOutcome;
use zupflow_core::runs::{NewRun, RunTrigger};
use zupflow_core::time::Timestamp;

/// The installation and tool a fixture run calls.
pub struct Target {
    pub installation_id: InstallationId,
    pub installation_short_id: String,
    pub installation_label: String,
    pub app_key: String,
    pub app_name: String,
    /// The tool's MCP name.
    pub tool: String,
    /// The SHA-256 of the tool's pinned definition.
    pub definition_sha256: Sha256Digest,
}

/// One run of a fixture: the store, the run, and the worker's claim on it.
struct Seeding<'a> {
    runs: &'a dyn RunStore,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
    claimant: ClaimantId,
    claim: Option<u64>,
}

impl Seeding<'_> {
    async fn snapshot(&self) -> Result<Snapshot, String> {
        self.runs
            .find_snapshot(self.organization_id, self.workspace_id, self.run_id)
            .await
            .map_err(|error| format!("{error:?}"))?
            .ok_or_else(|| "the run is gone".to_owned())
    }

    fn facts(&self) -> Facts {
        Facts {
            claimant: Some(self.claimant),
            claim: self.claim,
            model_route: Some(RouteState::Usable {
                key_version: FIRST_KEY_VERSION,
            }),
            ..Facts::at(Timestamp::now())
        }
    }

    /// Plans one step with `facts`, applies it, and answers the I/O it allows.
    async fn step(&mut self, facts: Facts) -> Result<Io, String> {
        let snapshot = self.snapshot().await?;
        let plan = advance(&snapshot, &facts).map_err(|error| format!("{error:?}"))?;
        let Plan::Apply { request, then } = plan else {
            return Err(format!("nothing to apply: {plan:?}"));
        };
        match self
            .runs
            .apply(self.organization_id, self.workspace_id, &request)
            .await
            .map_err(|error| format!("{error:?}"))?
        {
            Applied::Committed(_) => {}
            Applied::Conflict(conflict) => return Err(format!("{conflict:?}")),
        }
        if let zupflow_core::runs::transition::Operation::Claim(claim) = request.operation() {
            self.claim = Some(claim.epoch);
        }
        Ok(then)
    }

    /// Prepares the next model attempt with `offer`, then commits `reply` as
    /// its answer, deciding each call `decision`.
    async fn answer(
        &mut self,
        offer: &zupflow_core::runs::names::Offer,
        reply: Reply,
        usage: TokenUsage,
        decision: &CallDecision,
    ) -> Result<(), String> {
        let Io::CallModel(asked) = self
            .step(Facts {
                offer: Some(offer.clone()),
                ..self.facts()
            })
            .await?
        else {
            return Err("the model wasn't asked".to_owned());
        };
        let calls = match &reply {
            Reply::ToolCalls { calls, .. } => calls.len(),
            Reply::Text(_) => 0,
        };
        let decisions = (0..calls)
            .map(|position| (u32::try_from(position).unwrap_or(u32::MAX), decision.clone()))
            .collect();
        self.step(Facts {
            model: Some(ModelAnswer {
                turn: asked.turn,
                attempt: asked.attempt,
                result: ModelResult::Answered(Completion { reply, usage }),
            }),
            decisions,
            call_ids: (0..calls).map(|_| ToolCallId::generate()).collect(),
            ..self.facts()
        })
        .await?;
        Ok(())
    }
}

/// A run of `agent_id` that called `target`'s tool once: finished, or
/// waiting for approval of the call.
pub async fn seed(
    runs: &dyn RunStore,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    agent_id: AgentId,
    model: RunModel,
    target: Target,
    waiting: bool,
) -> Result<(), String> {
    let run_id = RunId::generate();
    let tool = target.tool.clone();
    let admission = admit(
        organization_id,
        workspace_id,
        run_id,
        &NewRun {
            agent_id,
            session_id: SessionId::generate(),
            trigger: RunTrigger::Api,
            input: format!("Use {tool} to find this week's open items."),
            model,
            instructions: "Be brief.".to_owned(),
            channel: None,
        },
    );
    runs.apply(organization_id, workspace_id, &admission)
        .await
        .map_err(|error| format!("{error:?}"))?;
    let mut seeding = Seeding {
        runs,
        organization_id,
        workspace_id,
        run_id,
        claimant: ClaimantId::from_uuid(Uuid::now_v7()),
        claim: None,
    };
    seeding.step(seeding.facts()).await?;

    let built = build_offer([Candidate {
        installation_id: target.installation_id,
        installation_short_id: target.installation_short_id,
        installation_label: target.installation_label,
        app_key: target.app_key,
        app_name: target.app_name,
        tool: tool.clone(),
        description: format!("Use {tool}."),
        input_schema: json!({ "type": "object" }),
        definition_sha256: target.definition_sha256,
    }]);
    let offered = built
        .offer
        .tools()
        .first()
        .ok_or_else(|| format!("{tool} can't be offered"))?
        .name
        .clone();
    let decision = if waiting {
        CallDecision {
            verdict: CallVerdict::RequireApproval {
                reason: format!("The agent's grant requires approval for {tool}."),
            },
            policy: "configured".to_owned(),
            mode: Some("manual".to_owned()),
            rule: "grant_requires_approval".to_owned(),
        }
    } else {
        CallDecision {
            verdict: CallVerdict::Allow,
            policy: "configured".to_owned(),
            mode: Some("manual".to_owned()),
            rule: "allowed".to_owned(),
        }
    };
    let arguments: Map<String, Value> = [("query".to_owned(), json!("open items"))].into_iter().collect();
    seeding
        .answer(
            &built.offer,
            Reply::ToolCalls {
                text: Some(format!("Calling {tool}.")),
                calls: vec![ModelToolCall {
                    id: "fixture-1".to_owned(),
                    name: offered,
                    arguments,
                }],
            },
            // Read from and written to the provider's prompt cache.
            TokenUsage::new(420, 18, 0, 400),
            &decision,
        )
        .await?;
    let snapshot = seeding.snapshot().await?;
    let call = snapshot.calls.first().ok_or("the call isn't recorded")?;
    if waiting {
        // Parks on the call, its installation still there: waiting for an
        // API key's decision.
        seeding
            .step(Facts {
                send_check: Some(SendCheck {
                    call_id: call.id,
                    installation: InstallationCheck::Usable,
                    definition_sha256: Some(target.definition_sha256),
                    decision: decision.clone(),
                }),
                ..seeding.facts()
            })
            .await?;
        return Ok(());
    }
    let Io::SendCall(send) = seeding
        .step(Facts {
            send_check: Some(SendCheck {
                call_id: call.id,
                installation: InstallationCheck::Usable,
                definition_sha256: Some(target.definition_sha256),
                decision: decision.clone(),
            }),
            invocation_id: Some(InvocationId::from_uuid(Uuid::now_v7())),
            ..seeding.facts()
        })
        .await?
    else {
        return Err("the call wasn't authorized".to_owned());
    };
    let result = json!({ "content": [{ "type": "text", "text": "3 open items" }] });
    let Value::Object(result) = result else {
        return Err("a result that isn't an object".to_owned());
    };
    seeding
        .step(Facts {
            sent: Some(SendResult {
                call_id: send.call_id,
                attempt: send.attempt,
                invocation_id: send.invocation_id,
                outcome: SendOutcome::Completed { result },
            }),
            ..seeding.facts()
        })
        .await?;
    seeding
        .answer(
            &built.offer,
            Reply::Text(format!("{tool}: 3 open items")),
            // Read from and written to the provider's prompt cache.
            TokenUsage::new(510, 12, 400, 90),
            &decision,
        )
        .await
}
