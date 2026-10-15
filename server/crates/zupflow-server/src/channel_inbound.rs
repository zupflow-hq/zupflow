//! A channel provider's deliveries, and the job that decides who answers a
//! new message.
//!
//! `POST /channels/inbound/{inbound_ref}` is called by the provider, with no
//! credential: the delivery's signature, by the app's own signing secret,
//! authorizes it. The timestamp is checked first, before the reference is
//! looked up; then a reference that names no app is checked against a
//! secret no app has, so it costs and answers exactly what a bad signature
//! does. Only a verified body is read. The app's reference is the one
//! cross-tenant read ([`ChannelStore::attribute_app_by_inbound_ref_cross_tenant`]);
//! everything after it names the organization and workspace it returned.
//! The provider's format is answered as it expects: `200` with the body
//! alone, no envelope.
//!
//! The store records the delivery once per app and, for a new message,
//! queues the inbound job ([`INBOUND_JOB`]) in the same transaction. The job
//! ([`work_once`]) follows the handling steps: it stops on the connection's
//! own message, builds the candidates (the agent channel on the
//! connection, when its agent is active and its rules admit the author:
//! those that need no one looked up first, then, when it has a sender list,
//! by the author's email read from the provider), asks the connection's
//! [`MessageHandler`], reads the names of the people a message that wakes
//! an agent names ([`crate::channel_reply::learn_people`]), and records
//! each dispatch in its own
//! transaction, which starts the session's run when it's idle, counted
//! against the conversation's breaker. Another agent's message waits, by
//! the job's retries, for its sender's record of how many hops led to it.
//! Then the message is marked handled. A message's text is never logged.
//! The job has its own worker loop ([`run_worker`]).
//!
//! [`ChannelStore::attribute_app_by_inbound_ref_cross_tenant`]: zupflow_core::channels::store::ChannelStore::attribute_app_by_inbound_ref_cross_tenant

use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::Deserialize;
use serde_json::json;
use zeroize::Zeroizing;
use zupflow_core::agents::AgentStatus;
use zupflow_core::channels::handling::{
    Author, Candidate, DispatchReason, HandlerInput, INBOUND_JOB, INBOUND_JOB_ATTEMPTS, InboundMessage, MentionHandler,
    MessageHandler, ThreadState, message_of_job,
};
use zupflow_core::channels::provider::{InboundDelivery, InboundRefusal, InboundRequest};
use zupflow_core::channels::store::{
    DeliveryOutcome, DispatchOutcome, InboundContext, NewDelivery, NewDispatch, SessionTemplate,
};
use zupflow_core::channels::thread::{instructions, origin_keys, preface};
use zupflow_core::channels::{
    AgentChannel, AgentMessages, ConversationKind, Direction, HandlerKind, InboundRef, Senders,
};
use zupflow_core::error::ApiError;
use zupflow_core::ids::{ChannelAppId, ChannelMessageId, OrganizationId, WorkspaceId};
use zupflow_core::jobs::Job;
use zupflow_core::keys::{self, EncryptionContext};
use zupflow_core::runs::history::history_budget;
use zupflow_core::scope::{Name, Scope};

use crate::auth::AppState;
use crate::http::Failure;
use crate::input::ProviderEventBody;
use crate::run_engine::AgentReads;

pub fn routes() -> Router<AppState> {
    Router::new().route("/channels/inbound/{inbound_ref}", post(receive))
}

/// What a reference that names no app is checked against, so it costs what
/// a known one does and answers the same.
const UNKNOWN_APP_SECRET: &[u8] = b"zupflow-unknown-inbound-reference";

/// How long a claim holds an inbound job: longer than reading the agent's
/// tools and one transaction per dispatch.
const LEASE: Duration = Duration::from_secs(60);

/// How long the inbound worker waits when no job is due.
const IDLE: Duration = Duration::from_millis(250);

fn stale() -> Failure {
    ApiError::new(
        401,
        "stale_request",
        "The request's timestamp is missing or more than five minutes off. Sign it with the current time.",
    )
    .into()
}

fn unsigned() -> Failure {
    ApiError::new(
        401,
        "invalid_signature",
        "The request isn't signed by the app it was sent to. Sign it with that app's signing secret.",
    )
    .into()
}

fn refusal(refused: InboundRefusal) -> Failure {
    match refused {
        InboundRefusal::Stale => stale(),
        InboundRefusal::InvalidSignature => unsigned(),
        InboundRefusal::InvalidJson => ApiError::new(400, "invalid_json", "The request body is not valid JSON.").into(),
        InboundRefusal::Unreadable => ApiError::new(
            400,
            "validation_failed",
            "The request isn't a provider event: it needs a type. Send events as the provider sends them.",
        )
        .into(),
    }
}

fn unavailable() -> Failure {
    ApiError::new(
        503,
        "unavailable",
        "The service is unavailable right now. Try again shortly.",
    )
    .into()
}

/// The provider's format: the body alone.
fn answer(challenge: Option<String>) -> Response {
    (StatusCode::OK, Json(json!({ "challenge": challenge }))).into_response()
}

/// An app's secrets as sealed together.
#[derive(Deserialize)]
struct OpenedSecrets {
    signing_secret: String,
}

/// The app a reference names, in its scope, with its signing secret: `None`
/// when it names none that can be checked against (unknown, deleted, or not
/// made yet). `Err` when the store or the keys can't be read.
async fn app_of(
    state: &AppState,
    inbound_ref: &str,
) -> Result<Option<(OrganizationId, WorkspaceId, ChannelAppId, Zeroizing<Vec<u8>>)>, Failure> {
    let Some(reference) = InboundRef::parse(inbound_ref) else {
        return Ok(None);
    };
    // Cross-tenant, by design: the reference is how the scope is learned.
    let Some((organization_id, workspace_id, app_id)) = state
        .channels
        .attribute_app_by_inbound_ref_cross_tenant(&reference)
        .await
        .map_err(|_| unavailable())?
    else {
        return Ok(None);
    };
    let (record, workspace) = tokio::join!(
        state.channels.app_secrets(organization_id, workspace_id, app_id),
        state.workspaces.find_workspace(organization_id, workspace_id),
    );
    let (Some(record), Some(workspace)) = (
        record.map_err(|_| unavailable())?,
        workspace.map_err(|_| unavailable())?,
    ) else {
        return Ok(None);
    };
    let Some(secrets) = record.secrets else {
        return Ok(None);
    };
    let context = EncryptionContext::channel_app_secret(
        organization_id,
        workspace_id,
        workspace.environment,
        app_id,
        secrets.version,
    );
    let opened = keys::open(state.keys.as_ref(), &context, &secrets.sealed)
        .await
        .map_err(|error| {
            tracing::error!(event = "channels.inbound_open_failed", %organization_id, %workspace_id, %app_id, %error);
            unavailable()
        })?;
    let secrets: OpenedSecrets = serde_json::from_slice(opened.expose()).map_err(|_| {
        tracing::error!(event = "channels.inbound_secrets_unreadable", %organization_id, %workspace_id, %app_id);
        unavailable()
    })?;
    // The same buffer, wiped when it's dropped.
    let signing_secret = Zeroizing::new(secrets.signing_secret.into_bytes());
    Ok(Some((organization_id, workspace_id, app_id, signing_secret)))
}

async fn receive(
    State(state): State<AppState>,
    Path(inbound_ref): Path<String>,
    headers: HeaderMap,
    body: ProviderEventBody,
) -> Result<Response, Failure> {
    let provider = state.channel_provider.as_ref();
    let names = provider.signature_headers();
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    let request = InboundRequest {
        timestamp: header(names.timestamp),
        signature: header(names.signature),
        body: body.raw(),
    };
    let now = state.clock.now();
    // Before anything is looked up.
    provider.check_timestamp(&request, now).map_err(refusal)?;
    let app = app_of(&state, &inbound_ref).await?;
    let secret: &[u8] = app
        .as_ref()
        .map_or(UNKNOWN_APP_SECRET, |(.., secret)| secret.as_slice());
    let verified = provider.verify(&request, secret, now);
    let Some((organization_id, workspace_id, app_id, _)) = app else {
        return Err(unsigned());
    };
    verified.map_err(refusal)?;
    let (event_id, kind, team_id, event) = match provider.read(request.body, now).map_err(refusal)? {
        InboundDelivery::UrlVerification { challenge } => return Ok(answer(Some(challenge))),
        InboundDelivery::Ignored => return Ok(answer(None)),
        InboundDelivery::Event {
            event_id,
            kind,
            team_id,
            event,
        } => (event_id, kind, team_id, event),
    };
    let delivery = NewDelivery {
        provider_event_id: event_id,
        kind,
        team_id,
        event,
    };
    // Everything from here is in the app's own scope.
    let outcome = state
        .channels
        .record_delivery(organization_id, workspace_id, app_id, &delivery, now)
        .await
        .map_err(|error| {
            tracing::warn!(event = "channels.inbound_not_recorded", %organization_id, %workspace_id, %app_id, %error);
            unavailable()
        })?;
    match outcome {
        DeliveryOutcome::Message {
            connection_id,
            message_id,
            new: true,
        } => {
            tracing::info!(event = "channels.message_received", %organization_id, %workspace_id, %app_id, %connection_id, %message_id);
        }
        DeliveryOutcome::Uninstalled => {
            tracing::info!(event = "channels.app_uninstalled", %organization_id, %workspace_id, %app_id);
        }
        DeliveryOutcome::Duplicate | DeliveryOutcome::Ignored | DeliveryOutcome::Message { .. } => {}
    }
    Ok(answer(None))
}

// --- The inbound job ----------------------------------------------------------

/// The handler a connection names.
fn handler_for(kind: HandlerKind) -> &'static dyn MessageHandler {
    match kind {
        HandlerKind::Mention => &MentionHandler,
    }
}

/// Another agent wrote the message, and its connection hasn't recorded
/// sending it yet: how many hops led to it isn't known.
struct HopUnknown;

/// The agent channel a message reaches, as the handler's candidate, with
/// the hops that led to the message.
type Admitted<'a> = (&'a AgentChannel, Candidate, u32);

/// Whether the message's author is of another organization's team: by the
/// team the provider named for them, or, when it named none, by the
/// conversation being shared with one.
fn is_external(context: &InboundContext) -> bool {
    match &context.author_team_id {
        Some(team) => *team != context.connection.team.id,
        None => context.conversation.is_external,
    }
}

/// The agent channel on the message's connection, as a candidate, when its
/// agent is active and its rules admit the author here; with the hops that
/// led to the message. The handler never sees an agent the author may not
/// reach. `Err` when another agent wrote it and its hops aren't known yet.
/// `senders`: whether to check the sender list too, which needs the
/// author's email; without it, the rules that need no one looked up.
fn candidate(context: &InboundContext, senders: bool) -> Result<Option<Admitted<'_>>, HopUnknown> {
    let Some(channel) = context.channel.as_ref() else {
        return Ok(None);
    };
    if !context.agent_active || context.message.deleted_at.is_some() {
        return Ok(None);
    }
    let rules = &channel.settings;
    // People of other organizations in a shared conversation wake it only
    // when it opts in.
    if is_external(context) && !rules.allow_external {
        return Ok(None);
    }
    let hop = match context.author_agent {
        // Another agent of the workspace: only as its settings allow, and
        // only under the hop limit, so agents can't wake each other forever.
        Some((author, hop)) => {
            if author == channel.id || rules.agent_messages == AgentMessages::Ignore {
                return Ok(None);
            }
            let hop = hop.ok_or(HopUnknown)?;
            if hop >= rules.max_agent_hops {
                return Ok(None);
            }
            hop
        }
        None => {
            let Some(author) = context.message.author.as_ref() else {
                return Ok(None);
            };
            if author.is_automated && !rules.allow_automated {
                return Ok(None);
            }
            0
        }
    };
    if senders && rules.senders == Senders::Listed {
        // By the email the connection knows the author by: no email, no match.
        let Some(email) = context.author_email.as_deref().map(str::trim) else {
            return Ok(None);
        };
        if !rules
            .sender_list
            .iter()
            .any(|listed| listed.trim().eq_ignore_ascii_case(email))
        {
            return Ok(None);
        }
    }
    Ok(Some((
        channel,
        Candidate {
            agent_channel_id: channel.id,
            agent_id: channel.agent_id,
            identity: context.connection.identity.external_id.clone(),
            answers_dms: rules.answers_dms,
            answers_mentions: rules.answers_mentions,
            thread_follow_up: rules.thread_follow_up,
            in_thread: context.in_thread,
        },
        hop,
    )))
}

/// What the agent's runs in a thread are fixed with, read now: its model,
/// the channel preface before its instructions, its tools and the history
/// budget its model's context window allows. `None` when it has no model.
async fn template(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    context: &InboundContext,
    channel: &AgentChannel,
) -> Result<Option<SessionTemplate>, &'static str> {
    let Some(agent) = state
        .agents
        .find_agent(organization_id, workspace_id, channel.agent_id)
        .await
        .map_err(|_| "store_unavailable")?
    else {
        return Ok(None);
    };
    if agent.status != AgentStatus::Active {
        return Ok(None);
    }
    let Some(model) = state
        .model_store
        .run_model(organization_id, workspace_id, agent.model_id)
        .await
        .map_err(|_| "store_unavailable")?
    else {
        return Ok(None);
    };
    let Some(workspace) = state
        .workspaces
        .find_workspace(organization_id, workspace_id)
        .await
        .map_err(|_| "store_unavailable")?
    else {
        return Ok(None);
    };
    let context_window = state
        .model_store
        .find_model(organization_id, Some(workspace.environment), model.model_id)
        .await
        .map_err(|_| "store_unavailable")?
        .and_then(|found| found.context_window);
    let offer = AgentReads {
        state,
        organization_id,
        workspace_id,
        agent_id: agent.id,
    }
    .offer()
    .await?;
    let preface = preface(
        context.connection.provider,
        &context.connection.identity.handle,
        context.conversation.kind,
    );
    Ok(Some(SessionTemplate {
        model,
        instructions: instructions(&preface, &agent.instructions),
        offer,
        history_budget: history_budget(context_window),
    }))
}

/// Decides who answers one message and records each dispatch. `Err` with
/// why when it's worth trying again: every step is idempotent. Another
/// agent's message whose hops aren't known yet is tried again, until the
/// `last` attempt, which wakes no one with it.
async fn handle_message(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    message_id: ChannelMessageId,
    last: bool,
) -> Result<(), &'static str> {
    let Some(context) = state
        .channels
        .inbound_context(organization_id, workspace_id, message_id)
        .await
        .map_err(|_| "store_unavailable")?
    else {
        // Gone with its connection or by retention.
        return Ok(());
    };
    if context.handled {
        return Ok(());
    }
    // The rules that need no one looked up come first: no one is read from
    // the provider, or saved, for a message no agent here may answer.
    let admissible = match candidate(&context, false) {
        Ok(admitted) => admitted.is_some(),
        Err(HopUnknown) if !last => return Err("agent_hop_unknown"),
        Err(HopUnknown) => {
            tracing::warn!(event = "channels.agent_hop_unknown", %organization_id, %workspace_id, %message_id);
            false
        }
    };
    // A sender list matches the author's email as the provider said it
    // within the day: an older one isn't trusted. While it can't be read,
    // the job tries again; on its last attempt, the message wakes no one.
    let listed = context
        .channel
        .as_ref()
        .is_some_and(|channel| channel.settings.senders == Senders::Listed);
    let mut verified = true;
    let context = match (&context.author_external_id, context.author_agent) {
        (Some(author), None) if admissible && listed => {
            let author = std::slice::from_ref(author);
            let learned =
                crate::channel_reply::learn_people(state, organization_id, workspace_id, &context, &[], author).await;
            if learned.unread {
                if !last {
                    return Err("sender_unverified");
                }
                tracing::warn!(event = "channels.sender_unverified", %organization_id, %workspace_id, %message_id);
                verified = false;
            }
            if learned.saved {
                match state
                    .channels
                    .inbound_context(organization_id, workspace_id, message_id)
                    .await
                    .map_err(|_| "store_unavailable")?
                {
                    Some(context) => context,
                    None => return Ok(()),
                }
            } else {
                context
            }
        }
        _ => context,
    };
    let admitted = if admissible && verified {
        candidate(&context, true).unwrap_or(None)
    } else {
        None
    };
    // The connection's own message is recorded only.
    if context.message.direction == Direction::Inbound
        && let Some((channel, candidate, hop)) = admitted
    {
        let message = &context.message;
        let (origin_key, thread_origin_key) = origin_keys(
            &context.connection.team.id,
            &context.conversation.external_id,
            &message.external_id,
            &message.thread_key,
        );
        let author = match (context.author_agent, &message.author) {
            (Some((agent_channel_id, _)), _) => Author::Agent { agent_channel_id, hop },
            (None, Some(person)) if person.is_automated => Author::Automated(person.id),
            (None, Some(person)) => Author::Person(person.id),
            (None, None) => return Err("message_without_author"),
        };
        let inbound = InboundMessage {
            id: message.id,
            origin_key: origin_key.clone(),
            thread_key: message.thread_key.clone(),
            author,
            is_direct: context.conversation.kind == ConversationKind::Dm,
            mentions: message.mentions.clone(),
            text: message.text.clone(),
        };
        let thread = ThreadState {
            last_addressed: context.last_addressed,
        };
        let candidates = [candidate];
        let handler = handler_for(context.connection.handler);
        let mut named = false;
        let dispatches = handler
            .decide(&HandlerInput {
                message: &inbound,
                thread: &thread,
                candidates: &candidates,
            })
            .await
            .map_err(|_| "handler_failed")?;
        for dispatch in dispatches {
            // The handler proposes; only a candidate is dispatched, and
            // another agent's message wakes this one only by a mention.
            if dispatch.agent_channel_id != channel.id
                || (context.author_agent.is_some() && dispatch.reason != DispatchReason::Mentioned)
            {
                continue;
            }
            let Some(template) = template(state, organization_id, workspace_id, &context, channel).await? else {
                tracing::warn!(event = "channels.no_model", %organization_id, %workspace_id, %message_id, agent_channel_id = %channel.id);
                continue;
            };
            // It wakes the agent: the run reads its author and the people it
            // mentions by name, so they're read first, once.
            if !named {
                named = true;
                let names: Vec<String> = context
                    .author_external_id
                    .iter()
                    .chain(message.mentions.iter())
                    .cloned()
                    .collect();
                crate::channel_reply::learn_people(state, organization_id, workspace_id, &context, &names, &[]).await;
            }
            let outcome = state
                .channels
                .dispatch(
                    organization_id,
                    workspace_id,
                    &NewDispatch {
                        message_id,
                        agent_channel_id: channel.id,
                        origin_key: origin_key.clone(),
                        thread_origin_key: thread_origin_key.clone(),
                        agent_hop: hop,
                        handler: handler.kind(),
                        reason: dispatch.reason,
                    },
                    Some(&template),
                    state.clock.now(),
                )
                .await
                .map_err(|error| {
                    tracing::warn!(event = "channels.dispatch_failed", %organization_id, %workspace_id, %message_id, %error);
                    "store_unavailable"
                })?;
            let reason = dispatch.reason.as_str();
            let agent_channel_id = channel.id;
            match outcome {
                DispatchOutcome::Started { run_id } => {
                    tracing::info!(event = "channels.run_started", %organization_id, %workspace_id, %message_id, %agent_channel_id, %run_id, reason);
                }
                DispatchOutcome::Waiting => {
                    tracing::info!(event = "channels.dispatch_waiting", %organization_id, %workspace_id, %message_id, %agent_channel_id, reason);
                }
                DispatchOutcome::Paused { until, tripped } => {
                    tracing::info!(event = "channels.dispatch_paused", %organization_id, %workspace_id, %message_id, %agent_channel_id, until = %until, tripped);
                }
                DispatchOutcome::AlreadyRead => {
                    tracing::info!(event = "channels.dispatch_already_read", %organization_id, %workspace_id, %message_id, %agent_channel_id, reason);
                }
                DispatchOutcome::Inactive | DispatchOutcome::Duplicate => {}
            }
        }
    }
    state
        .channels
        .mark_handled(organization_id, workspace_id, message_id, state.clock.now())
        .await
        .map_err(|_| "store_unavailable")
}

/// Claims and handles one inbound job, if one is due. Whether one was.
pub async fn work_once(state: &AppState) -> bool {
    let Ok(kind) = Name::new(INBOUND_JOB) else {
        return false;
    };
    // Cross-tenant by design: a worker takes the next job of any scope.
    let job = match state.jobs.claim_next(std::slice::from_ref(&kind), LEASE).await {
        Ok(Some(job)) => job,
        Ok(None) => return false,
        Err(error) => {
            tracing::warn!(event = "channels.inbound_claim_failed", %error);
            return false;
        }
    };
    handle(state, job).await;
    true
}

/// Handles one claimed inbound job: completed, tried again later, or
/// buried after its last attempt.
pub async fn handle(state: &AppState, job: Job) {
    let Job {
        id,
        scope,
        payload,
        attempt,
        lease,
        ..
    } = job;
    let (Scope::Workspace(organization_id, workspace_id), Some(message_id)) = (scope, message_of_job(&payload)) else {
        tracing::error!(event = "channels.malformed_job", kind = INBOUND_JOB);
        if let Ok(reason) = Name::new("malformed") {
            let _ = state.jobs.bury(&scope, id, &lease, &reason).await;
        }
        return;
    };
    let last = attempt >= INBOUND_JOB_ATTEMPTS;
    let finished = match handle_message(state, organization_id, workspace_id, message_id, last).await {
        Ok(()) => state.jobs.complete(&scope, id, &lease).await,
        Err(why) => {
            tracing::warn!(event = "channels.inbound_retry", %organization_id, %workspace_id, %message_id, why, attempt);
            match Name::new(why) {
                Ok(reason) => {
                    let after = Duration::from_secs(5 * u64::from(attempt.max(1)));
                    state.jobs.retry(&scope, id, &lease, after, &reason).await.map(|_| ())
                }
                Err(_) => state.jobs.complete(&scope, id, &lease).await,
            }
        }
    };
    if let Err(error) = finished {
        tracing::warn!(event = "channels.inbound_finish_failed", %organization_id, %workspace_id, %message_id, %error);
    }
}

/// Answers new messages until the process stops: a loop of its own, so
/// nothing slower (making apps at the provider) holds them up for anyone.
pub async fn run_worker(state: AppState) {
    loop {
        if !work_once(&state).await {
            tokio::time::sleep(IDLE).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_a_refusal_with_the_mocks_codes() {
        for (refused, status, code) in [
            (InboundRefusal::Stale, 401, "stale_request"),
            (InboundRefusal::InvalidSignature, 401, "invalid_signature"),
            (InboundRefusal::InvalidJson, 400, "invalid_json"),
            (InboundRefusal::Unreadable, 400, "validation_failed"),
        ] {
            let Failure(error) = refusal(refused);
            assert_eq!((error.status, error.code), (status, code));
        }
    }
}
