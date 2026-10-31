//! A run's memory: its agent's memory type, through the type's kind
//! ([`MemoryProvider`]). Nothing here knows a kind: what a kind recalls,
//! which tools it has and what a call does are the kind's own.
//!
//! **Frozen with the tools.** A run's memory is read once, for its first
//! model attempt, with the rest of its tools ([`resolve`]): the agent's type
//! (its own, or the organization's default), the kind's tools for the type's
//! settings and `writes`, and what the run recalls, for the workspace. The
//! tools join the offer by their own names, and the type and the recall are
//! kept on the run, once ([`FrozenMemory`]), so every later attempt (a retry,
//! another worker, a turn after an approval) sends the same tools and the
//! same recall, byte for byte. Nothing reads the recall again during a run:
//! what a run saves reaches the next one. An agent with no memory, or a type
//! whose kind this server doesn't serve: no tools, no recall. A run started
//! with memory `off` offers the same tools, so its prompt and its prompt
//! cache key are an `agent` run's, recalls nothing, and every call to one is
//! refused (`memory_off`).
//!
//! **Decided live.** Each call is decided when the model makes it, and again
//! just before it's sent, from the run's type as it is then ([`decide`]): a
//! tool that reads is allowed; one that writes takes the type's `writes`
//! (`allow`; `approve`, and the run waits for a person, as on any
//! approve-level call; `deny`, refused as read-only). The type is the one
//! the run started with, never another: once it's deleted, a call is refused
//! (`memory_type_removed`), and so is one already waiting for a person
//! (the job the type's deletion queues). A call is refused for one reason
//! with one code, whenever it's refused: decided, checked before it's sent,
//! or made (`memory_kind_not_served` for a kind this server doesn't serve,
//! `invalid_memory_settings` for settings its kind refuses now, and a tool
//! the kind doesn't have as an app's unknown tool is).
//!
//! **Made in the run's scope.** A call reaches only the run's organization,
//! workspace and agent, in its type ([`MemoryScope`]), with the type's
//! `writes` as they are when it's made, and the run and the call as the
//! origin of what it writes, saying whether the model had read an app's tool
//! result before it ([`after_app_content`]). A change commits its
//! `memory.saved`, `memory.updated` or `memory.forgotten` webhook events in
//! its own transaction ([`Record`]). The engine audits no tool call, a memory
//! tool's neither: the run's timeline is the record. No log line holds a
//! memory's text, a call's arguments or its answer.
//!
//! **Once.** A call that changes a memory commits a record of itself with
//! its change, one for each call ([`Recorded::call`]), and one that gives a
//! memory the text it holds commits the record alone. A call whose sender
//! stopped before recording what came of it is settled by that record
//! ([`MemoryProvider::made`]): answered as it was made, whatever became of
//! the memory or its type since, and its change never made again; with
//! none (a search, a refusal, or a call never made), it's made then, as
//! [`memory::settle`] does. A sender that commits after its call was
//! settled is refused, changing nothing ([`memory::make`]). A call of a kind
//! that keeps no record, or that this server doesn't serve, is settled
//! unknown.
//!
//! [`Record`]: zupflow_core::memory::Record
//! [`Recorded::call`]: zupflow_core::memory::Recorded::call

use std::sync::Arc;

use serde_json::{Map, Value};
use zupflow_core::ids::{AgentId, MemoryTypeId, OrganizationId, RunId, ToolCallId, WorkspaceId};
use zupflow_core::memory::type_store::MemoryType;
use zupflow_core::memory::{
    self, Audience, CallContext, CallResult, MemoryChange, MemoryError, MemoryKind, MemoryProvider, MemoryScope,
    MemoryTool, MemoryWrites, Origin, Recorded, Settings,
};
use zupflow_core::policy::{Rule, ToolLevel};
use zupflow_core::runs::MemoryMode;
use zupflow_core::runs::digest::{Sha256Digest, memory_call_digest};
use zupflow_core::runs::names::{OfferedTool, memory_tool};
use zupflow_core::runs::reducer::{
    InstallationCheck, MEMORY_KIND_NOT_SERVED, MEMORY_KIND_NOT_SERVED_MESSAGE, MEMORY_OFF, MEMORY_OFF_MESSAGE,
    MEMORY_SETTINGS_INVALID, MEMORY_SETTINGS_INVALID_MESSAGE, MEMORY_TYPE_REMOVED, MEMORY_TYPE_REMOVED_MESSAGE,
    MemorySendRequest, SendCheck, after_app_content, refusal_code,
};
use zupflow_core::runs::state::{CallDecision, CallRecord, CallVerdict, FrozenMemory, Snapshot};
use zupflow_core::runs::transition::SendOutcome;
use zupflow_core::workspaces::Workspace;

use crate::auth::AppState;
use crate::memories::memory_event_in;

/// The policy a memory call's decision names.
pub const POLICY: &str = "memory";

/// What the model reads back for a call that writes while the type's
/// `writes` are `deny`.
pub const READ_ONLY: &str = "This agent's memory is read-only now, so it can't be changed.";

/// The code of a call that couldn't be made just now: nothing changed.
const UNAVAILABLE: &str = "memory_unavailable";

const UNAVAILABLE_MESSAGE: &str = "The memory couldn't be reached just now, so nothing was changed. Try again.";

fn decision(verdict: CallVerdict, rule: &str) -> CallDecision {
    CallDecision {
        verdict,
        policy: POLICY.to_owned(),
        mode: None,
        rule: rule.to_owned(),
    }
}

fn deny(rule: &str, reason: impl Into<String>) -> CallDecision {
    decision(CallVerdict::Deny { reason: reason.into() }, rule)
}

fn refused(code: &str, message: &str) -> SendOutcome {
    SendOutcome::Refused {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

// --- The type as it is now -------------------------------------------------------

/// A run's memory type as it is now.
enum TypeNow {
    /// Deleted.
    Gone,
    /// This server doesn't serve its kind.
    NotServed,
    /// Its kind refuses the settings it holds, which it kept when the type
    /// was saved: a server that changed what it accepts.
    Unusable,
    Served(Served),
}

struct Served {
    memory_type: MemoryType,
    provider: Arc<dyn MemoryProvider>,
    settings: Settings,
}

impl Served {
    /// Every tool the kind has for the type's settings, whatever its
    /// `writes`: what a call's definition is held to.
    fn definition(&self, tool: &str) -> Option<zupflow_core::runs::digest::Sha256Digest> {
        self.provider
            .tools(&self.settings, MemoryWrites::Allow)
            .iter()
            .find(|offered| offered.name == tool)
            .map(MemoryTool::definition_sha256)
    }

    /// The scope of a run of `agent_id` in this type.
    fn scope(&self, organization_id: OrganizationId, workspace_id: WorkspaceId, agent_id: AgentId) -> MemoryScope {
        MemoryScope {
            organization_id,
            workspace_id,
            agent_id,
            memory_type_id: self.memory_type.id,
        }
    }
}

/// The organization's type `memory_type_id`, read now, with its kind and
/// its settings as the kind checks them.
async fn type_now(
    state: &AppState,
    organization_id: OrganizationId,
    memory_type_id: MemoryTypeId,
) -> Result<TypeNow, &'static str> {
    let Some(memory_type) = state
        .memory_types
        .find_type(organization_id, memory_type_id)
        .await
        .map_err(|_| "store_unavailable")?
    else {
        return Ok(TypeNow::Gone);
    };
    if !state.memory_providers.serves(&memory_type.kind) {
        return Ok(TypeNow::NotServed);
    }
    let provider = state.memory_providers.provider(&memory_type.kind);
    let Some(settings) = stored_settings(provider.as_ref(), organization_id, &memory_type) else {
        return Ok(TypeNow::Unusable);
    };
    Ok(TypeNow::Served(Served {
        memory_type,
        provider,
        settings,
    }))
}

/// The type's settings as stored, checked again by its kind, which kept them
/// when the type was saved: `None` when it refuses them now, the server's to
/// fix. Only the fields' names are logged.
fn stored_settings(
    provider: &dyn MemoryProvider,
    organization_id: OrganizationId,
    memory_type: &MemoryType,
) -> Option<Settings> {
    provider
        .check_settings(&memory_type.settings)
        .inspect_err(|refusal| {
            tracing::error!(
                event = "runs.memory_settings_refused",
                %organization_id,
                memory_type_id = %memory_type.id,
                kind = memory_type.kind.as_str(),
                settings = ?refusal.fields.keys().collect::<Vec<_>>(),
            );
        })
        .ok()
}

// --- What a run starts with -------------------------------------------------------

/// What a run's first model attempt freezes of its memory: the tools its
/// type offers, to join the rest by their own names, and the type with what
/// the run recalls.
pub(crate) struct ResolvedMemory {
    pub(crate) tools: Vec<OfferedTool>,
    pub(crate) frozen: FrozenMemory,
}

/// The memory a run of `agent_id` starts with, read now for its first model
/// attempt: `None` when the agent has none (its memory is off, or it's on
/// the organization's default and there's none) or this server doesn't
/// serve its type's kind. A run started with memory `off` gets the same
/// tools and recalls nothing. A type or a recall that can't be read, or
/// settings its kind refuses now, fail the step, to be tried again, as an
/// offer that can't be read does: a run never starts without the memory its
/// agent has.
pub(crate) async fn resolve(
    state: &AppState,
    (organization_id, workspace_id): (OrganizationId, WorkspaceId),
    agent_id: AgentId,
    mode: MemoryMode,
) -> Result<Option<ResolvedMemory>, &'static str> {
    let Some(run_memory) = state
        .memory_types
        .run_memory(organization_id, workspace_id, agent_id)
        .await
        .map_err(|_| "store_unavailable")?
    else {
        return Ok(None);
    };
    let served = match type_now(state, organization_id, run_memory.memory_type_id).await? {
        TypeNow::Served(served) => served,
        TypeNow::NotServed => return Ok(None),
        // Deleted since the agent's memory was read: read it again.
        TypeNow::Gone => return Err("memory_changed"),
        // The server's to fix: no model was asked yet, so nothing is lost
        // trying again.
        TypeNow::Unusable => return Err("memory_settings_refused"),
    };
    let kind_name = served.provider.describe().name;
    let memory_type = &served.memory_type;
    let tools = served
        .provider
        .tools(&served.settings, memory_type.writes)
        .iter()
        .map(|tool| {
            memory_tool(
                memory_type.id,
                &memory_type.name,
                memory_type.kind.as_str(),
                &kind_name,
                tool,
            )
        })
        .collect();
    let recall = match mode {
        MemoryMode::Off => None,
        MemoryMode::Agent => {
            let scope = served.scope(organization_id, workspace_id, agent_id);
            let recall = served
                .provider
                .recall(&scope, &Audience::Workspace, &served.settings)
                .await
                .map_err(|error| {
                    tracing::warn!(event = "runs.memory_unreadable", %organization_id, %workspace_id, %agent_id, memory_type_id = %memory_type.id, %error);
                    "memory_unavailable"
                })?;
            recall.framed()
        }
    };
    Ok(Some(ResolvedMemory {
        tools,
        frozen: FrozenMemory {
            memory_type_id: memory_type.id,
            recall,
        },
    }))
}

// --- Decisions ---------------------------------------------------------------------

/// The decision on a call to `tool` of the run's memory type, read now:
/// refused in a run started with memory `off`, once the type is deleted,
/// while its kind isn't served or refuses its settings, and for a tool its
/// kind doesn't have; a tool that reads, allowed; one that writes, at the
/// type's `writes` now. Only a store that can't be read fails the step.
pub(crate) async fn decide(
    state: &AppState,
    organization_id: OrganizationId,
    memory: MemoryMode,
    memory_type_id: MemoryTypeId,
    tool: &str,
) -> Result<CallDecision, &'static str> {
    if memory == MemoryMode::Off {
        return Ok(deny(MEMORY_OFF, MEMORY_OFF_MESSAGE));
    }
    let now = type_now(state, organization_id, memory_type_id).await?;
    Ok(decision_now(memory, &now, tool))
}

fn decision_now(memory: MemoryMode, now: &TypeNow, tool: &str) -> CallDecision {
    if memory == MemoryMode::Off {
        return deny(MEMORY_OFF, MEMORY_OFF_MESSAGE);
    }
    let served = match now {
        TypeNow::Gone => return deny(MEMORY_TYPE_REMOVED, MEMORY_TYPE_REMOVED_MESSAGE),
        TypeNow::NotServed => return deny(MEMORY_KIND_NOT_SERVED, MEMORY_KIND_NOT_SERVED_MESSAGE),
        TypeNow::Unusable => return deny(MEMORY_SETTINGS_INVALID, MEMORY_SETTINGS_INVALID_MESSAGE),
        TypeNow::Served(served) => served,
    };
    level_decision(
        tool,
        &served.provider.tools(&served.settings, served.memory_type.writes),
        &served.provider.tools(&served.settings, MemoryWrites::Allow),
    )
}

/// A call to `tool` decided by the tools its type offers at its `writes`
/// now, `offered`, among every tool its kind has, `every`: at the level the
/// kind gives it, refused as read-only when it writes and isn't offered,
/// and refused as an app's tool its installation doesn't have is
/// (`unknown_tool`) when the kind has no such tool.
fn level_decision(tool: &str, offered: &[MemoryTool], every: &[MemoryTool]) -> CallDecision {
    let level = match offered.iter().find(|offered| offered.name == tool) {
        Some(offered) => offered.level,
        // A tool that writes isn't offered while the type's writes are denied.
        None if every.iter().any(|known| known.name == tool) => ToolLevel::Deny,
        None => {
            return deny(
                Rule::UnknownTool.as_str(),
                format!("{tool} isn't one of this memory's tools."),
            );
        }
    };
    match level {
        ToolLevel::Allow => decision(CallVerdict::Allow, Rule::Allowed.as_str()),
        ToolLevel::Approve => decision(
            CallVerdict::RequireApproval {
                reason: format!("Your organization requires approval for {tool}."),
            },
            Rule::OrganizationRequiresApproval.as_str(),
        ),
        ToolLevel::Deny => deny(Rule::OrganizationDenies.as_str(), READ_ONLY),
    }
}

/// A memory call's type and recorded decision, read for a check: `Err`
/// for a call that isn't one.
async fn checked(
    state: &AppState,
    organization_id: OrganizationId,
    call: &CallRecord,
) -> Result<(TypeNow, InstallationCheck), &'static str> {
    let (Some(target), Some(_)) = (&call.target, &call.decision) else {
        return Err("run_inconsistent");
    };
    let Some(memory_type_id) = target.memory() else {
        return Err("run_inconsistent");
    };
    let now = type_now(state, organization_id, memory_type_id).await?;
    let installation = match now {
        TypeNow::Gone => InstallationCheck::Removed,
        // Refused by the decision, with its own code.
        TypeNow::NotServed | TypeNow::Unusable | TypeNow::Served(_) => InstallationCheck::Usable,
    };
    Ok((now, installation))
}

/// A memory call checked again just before it's sent: its type, its
/// decision read now and the tool's definition now.
pub(crate) async fn send_check(
    state: &AppState,
    organization_id: OrganizationId,
    memory: MemoryMode,
    call: &CallRecord,
) -> Result<SendCheck, &'static str> {
    let (now, installation) = checked(state, organization_id, call).await?;
    let tool = call.target.as_ref().map_or("", |target| target.tool.as_str());
    let definition_sha256 = match &now {
        TypeNow::Served(served) => served.definition(tool),
        TypeNow::Gone | TypeNow::NotServed | TypeNow::Unusable => None,
    };
    Ok(SendCheck {
        call_id: call.id,
        installation,
        definition_sha256,
        decision: decision_now(memory, &now, tool),
    })
}

/// A memory call waiting for approval, checked before the run parks on it:
/// only whether its type is still there counts, so nobody is asked about a
/// call that can't be made, and its decision stays the one recorded.
pub(crate) async fn parking_check(
    state: &AppState,
    organization_id: OrganizationId,
    call: &CallRecord,
) -> Result<SendCheck, &'static str> {
    let (_, installation) = checked(state, organization_id, call).await?;
    let (Some(target), Some(recorded)) = (&call.target, &call.decision) else {
        return Err("run_inconsistent");
    };
    Ok(SendCheck {
        call_id: call.id,
        installation,
        definition_sha256: Some(target.definition_sha256),
        decision: recorded.clone(),
    })
}

// --- Making calls ----------------------------------------------------------------

/// What the kind answered, as the call's outcome: its answer, or its own
/// error, for the model to read.
fn answered(result: CallResult) -> SendOutcome {
    let mut wrapped = Map::new();
    if result.is_error {
        wrapped.insert("error".to_owned(), Value::String(result.content));
        SendOutcome::ToolError { result: wrapped }
    } else {
        wrapped.insert("result".to_owned(), Value::String(result.content));
        SendOutcome::Completed { result: wrapped }
    }
}

/// What a run's change records with it: its webhook events, placed in the
/// run's workspace, and no audit event. The kind adds the call itself.
fn recorded(organization_id: OrganizationId, workspace: &Workspace, change: &MemoryChange) -> Recorded {
    Recorded {
        audit: None,
        events: vec![memory_event_in(organization_id, workspace, change)],
        call: None,
    }
}

/// One memory call to make, in its run's scope, as it was decided.
struct Call<'a> {
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
    call_id: ToolCallId,
    memory_type_id: MemoryTypeId,
    kind: &'a str,
    tool: &'a str,
    arguments: &'a Map<String, Value>,
    call_digest: Sha256Digest,
    definition_sha256: Sha256Digest,
    after_app_content: bool,
}

impl<'a> Call<'a> {
    fn sent(send: &'a MemorySendRequest) -> Self {
        Self {
            organization_id: send.organization_id,
            workspace_id: send.workspace_id,
            run_id: send.run_id,
            call_id: send.call_id,
            memory_type_id: send.memory_type_id,
            kind: &send.kind,
            tool: &send.tool,
            arguments: &send.arguments,
            call_digest: send.call_digest,
            definition_sha256: send.definition_sha256,
            after_app_content: send.after_app_content,
        }
    }
}

/// Makes one memory call, under its send claim, in the run's scope: its type
/// and digest checked once more, then the kind asked with the type's
/// `writes` as they are now.
pub(crate) async fn send(state: &AppState, agent_id: AgentId, send: &MemorySendRequest) -> SendOutcome {
    match make(state, agent_id, &Call::sent(send)).await {
        Ok(outcome) => outcome,
        Err(why) => {
            tracing::warn!(event = "runs.memory_call_not_made", organization_id = %send.organization_id, workspace_id = %send.workspace_id, run_id = %send.run_id, call_id = %send.call_id, memory_type_id = %send.memory_type_id, why);
            refused(UNAVAILABLE, UNAVAILABLE_MESSAGE)
        }
    }
}

/// Makes `call` now: refused as it would be decided now, each reason with
/// its own code, and as `tool_changed` when it no longer matches what was
/// decided; else made through its kind, which answers a call that made its
/// change already as it was made, changing nothing ([`memory::make`]).
async fn make(state: &AppState, agent_id: AgentId, call: &Call<'_>) -> Result<SendOutcome, &'static str> {
    let (organization_id, workspace_id) = (call.organization_id, call.workspace_id);
    let now = type_now(state, organization_id, call.memory_type_id).await?;
    // Only a run with memory gets this far: one with memory off is refused
    // as each call is decided.
    let decided = decision_now(MemoryMode::Agent, &now, call.tool);
    if let CallVerdict::Deny { reason } = &decided.verdict {
        return Ok(refused(refusal_code(&decided), reason));
    }
    let TypeNow::Served(served) = now else {
        return Err("run_inconsistent");
    };
    // Only the run's own type and the tool as it was offered and decided.
    let digest = memory_call_digest(
        call.memory_type_id,
        call.kind,
        call.tool,
        &call.definition_sha256,
        call.arguments,
    );
    if served.memory_type.kind.as_str() != call.kind
        || digest != call.call_digest
        || served.definition(call.tool) != Some(call.definition_sha256)
    {
        return Ok(refused(
            "tool_changed",
            "This call no longer matches what was decided, so it wasn't made.",
        ));
    }
    let workspace = state
        .workspaces
        .find_workspace(organization_id, workspace_id)
        .await
        .map_err(|_| "store_unavailable")?
        .ok_or("workspace_gone")?;
    let scope = served.scope(organization_id, workspace_id, agent_id);
    let origin = Origin::Run {
        run_id: call.run_id,
        tool_call_id: call.call_id,
        after_app_content: call.after_app_content,
    };
    let context = CallContext {
        origin: &origin,
        audience: &Audience::Workspace,
        settings: &served.settings,
        writes: served.memory_type.writes,
        now: state.clock.now(),
    };
    let record = |change: &MemoryChange| recorded(organization_id, &workspace, change);
    match memory::make(
        served.provider.as_ref(),
        &scope,
        &context,
        call.tool,
        call.arguments,
        &record,
    )
    .await
    {
        Ok(result) => Ok(answered(result)),
        // The run's agent or type went as the call was made.
        Err(MemoryError::NotFound) => Ok(refused(MEMORY_TYPE_REMOVED, MEMORY_TYPE_REMOVED_MESSAGE)),
        Err(MemoryError::NotServed) => Ok(refused(MEMORY_KIND_NOT_SERVED, MEMORY_KIND_NOT_SERVED_MESSAGE)),
        Err(_) => Err("memory_unavailable"),
    }
}

// --- A call whose sender stopped ----------------------------------------------------

/// Settles a memory call whose sender stopped before recording what came of
/// it, in the run's scope. A call that made its change is answered as its
/// change recorded it ([`MemoryProvider::made`]), whatever became of the
/// memory, or of its type, since; one that made none is made now, as it's
/// sent. `None` when what came of it can't be told, its kind keeping no
/// record of its calls or not served here: the call is settled unknown. A
/// store that can't be read fails the step, to be tried again.
pub(crate) async fn settle_lapsed(
    state: &AppState,
    (organization_id, workspace_id, run_id): (OrganizationId, WorkspaceId, RunId),
    agent_id: AgentId,
    snapshot: &Snapshot,
    call: &CallRecord,
) -> Result<Option<SendOutcome>, &'static str> {
    let Some(target) = call.target.as_ref() else {
        return Ok(None);
    };
    let (Some(memory_type_id), Some(kind), Some(call_digest)) =
        (target.memory(), MemoryKind::parse(&target.app_key), call.call_digest)
    else {
        return Ok(None);
    };
    if !state.memory_providers.serves(&kind) {
        return Ok(None);
    }
    // What it made, read first: its type may be gone since.
    let scope = MemoryScope {
        organization_id,
        workspace_id,
        agent_id,
        memory_type_id,
    };
    match state
        .memory_providers
        .provider(&kind)
        .made(&scope, run_id, call.id)
        .await
    {
        Ok(Some(made)) => return Ok(Some(answered(made.result()))),
        Ok(None) => {}
        Err(MemoryError::Unavailable) => return Err("store_unavailable"),
        Err(_) => return Ok(None),
    }
    let call = Call {
        organization_id,
        workspace_id,
        run_id,
        call_id: call.id,
        memory_type_id,
        kind: &target.app_key,
        tool: &target.tool,
        arguments: &call.arguments,
        call_digest,
        definition_sha256: target.definition_sha256,
        after_app_content: after_app_content(snapshot, call),
    };
    make(state, agent_id, &call).await.map(Some)
}

#[cfg(test)]
mod tests {
    use zupflow_core::memory::MemoryWrites;
    use zupflow_core::memory::notes::{self, FORGET_MEMORY, SAVE_MEMORY, SEARCH_MEMORY, UPDATE_MEMORY};
    use zupflow_core::policy::Rule;
    use zupflow_core::runs::MemoryMode;
    use zupflow_core::runs::reducer::{
        MEMORY_KIND_NOT_SERVED, MEMORY_KIND_NOT_SERVED_MESSAGE, MEMORY_OFF, MEMORY_OFF_MESSAGE,
        MEMORY_SETTINGS_INVALID, MEMORY_SETTINGS_INVALID_MESSAGE, MEMORY_TYPE_REMOVED, MEMORY_TYPE_REMOVED_MESSAGE,
        TOOL_REFUSED, refusal_code,
    };
    use zupflow_core::runs::state::CallVerdict;

    use super::{POLICY, READ_ONLY, TypeNow, decision_now, level_decision};

    #[test]
    fn decides_a_call_by_the_types_writes_now() {
        let every = notes::tools(MemoryWrites::Allow);
        for (writes, expected) in [
            (MemoryWrites::Allow, "allow"),
            (MemoryWrites::Approve, "require_approval"),
            (MemoryWrites::Deny, "deny"),
        ] {
            let offered = notes::tools(writes);
            for tool in [SAVE_MEMORY, UPDATE_MEMORY, FORGET_MEMORY] {
                let decided = level_decision(tool, &offered, &every);
                assert_eq!(decided.policy, POLICY);
                let verdict = match &decided.verdict {
                    CallVerdict::Allow => "allow",
                    CallVerdict::RequireApproval { reason } => {
                        assert_eq!(reason, &format!("Your organization requires approval for {tool}."));
                        "require_approval"
                    }
                    CallVerdict::Deny { reason } => {
                        assert_eq!(reason, READ_ONLY);
                        "deny"
                    }
                };
                assert_eq!(verdict, expected, "{tool} with writes {}", writes.as_str());
            }
            // A search only reads, whatever the writes.
            assert_eq!(
                level_decision(SEARCH_MEMORY, &offered, &every).verdict,
                CallVerdict::Allow
            );
            // A tool the kind doesn't have, as an app's tool its
            // installation doesn't have.
            let unknown = level_decision("erase_everything", &offered, &every);
            assert_eq!(
                (unknown.rule.as_str(), refusal_code(&unknown), &unknown.verdict),
                (
                    Rule::UnknownTool.as_str(),
                    TOOL_REFUSED,
                    &CallVerdict::Deny {
                        reason: "erase_everything isn't one of this memory's tools.".to_owned()
                    }
                )
            );
        }
    }

    /// Each reason a call is refused with its own code, the one the call
    /// fails with.
    #[test]
    fn refuses_every_call_with_memory_off_or_once_the_type_is_gone() {
        for tool in [SAVE_MEMORY, SEARCH_MEMORY] {
            for now in [TypeNow::Gone, TypeNow::NotServed, TypeNow::Unusable] {
                let off = decision_now(MemoryMode::Off, &now, tool);
                assert_eq!(off.rule, MEMORY_OFF);
                assert_eq!(
                    off.verdict,
                    CallVerdict::Deny {
                        reason: MEMORY_OFF_MESSAGE.to_owned()
                    }
                );
            }
            let gone = decision_now(MemoryMode::Agent, &TypeNow::Gone, tool);
            assert_eq!(gone.rule, MEMORY_TYPE_REMOVED);
            assert_eq!(
                gone.verdict,
                CallVerdict::Deny {
                    reason: MEMORY_TYPE_REMOVED_MESSAGE.to_owned()
                }
            );
            for (now, code, message) in [
                (TypeNow::Gone, MEMORY_TYPE_REMOVED, MEMORY_TYPE_REMOVED_MESSAGE),
                (
                    TypeNow::NotServed,
                    MEMORY_KIND_NOT_SERVED,
                    MEMORY_KIND_NOT_SERVED_MESSAGE,
                ),
                (
                    TypeNow::Unusable,
                    MEMORY_SETTINGS_INVALID,
                    MEMORY_SETTINGS_INVALID_MESSAGE,
                ),
            ] {
                let refused = decision_now(MemoryMode::Agent, &now, tool);
                assert_eq!(
                    (refusal_code(&refused), &refused.verdict),
                    (
                        code,
                        &CallVerdict::Deny {
                            reason: message.to_owned()
                        }
                    ),
                    "{tool}"
                );
            }
            assert_eq!(
                refusal_code(&decision_now(MemoryMode::Off, &TypeNow::Gone, tool)),
                MEMORY_OFF
            );
        }
    }
}
