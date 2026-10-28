//! What a connection asks its provider with its own token: the people a new
//! message names, before the inbound job decides who answers it, and a
//! channel run's answer, posted to its thread by the reply job.
//!
//! **People.** The inbound job reads people from the provider
//! ([`learn_people`]) only for a message its cheap rules admit (the agent
//! active, the author of a team it answers, automated or not as it allows):
//! the author's email when a sender list is checked, and once the message
//! wakes the agent, the names of its author and the first people it
//! mentions. The connection's people are its cache: a name, once known, is
//! kept and never read again; someone the provider doesn't know, and an
//! email a sender list is checked against, are read again after
//! [`PERSON_LOOKUP_TTL`]. Names are what a run reads each message's author
//! and mentions by, rendered once when it takes the message in. A read that
//! fails leaves the name unknown, and the run reads the provider's ID
//! instead; the next message tries again.
//!
//! [`PERSON_LOOKUP_TTL`]: zupflow_core::channels::reply::PERSON_LOOKUP_TTL
//!
//! **Replies.** The transaction that ends a channel run with an answer
//! queues [`REPLY_JOB`]; [`work_once`] posts it through the provider with
//! the connection's token, carrying the run's ID, and records the post as
//! the connection's outbound message with the run and its hops. A retried
//! job looks for a post whose answer was lost before posting again (and
//! never posts again when the provider can't look); a rate limit postpones
//! it without using an attempt, for a day after the run at most. A reply
//! left unposted is logged as `channels.reply_not_sent` with why. The
//! answer's text is never logged, nor the provider's words.

use std::time::Duration;

use zeroize::Zeroizing;
use zupflow_core::channels::provider::{OutgoingPost, ProviderCallError};
use zupflow_core::channels::reply::{ChannelReply, MAX_PEOPLE_READ, REPLY_JOB, reply_expired, run_of_job};
use zupflow_core::channels::store::{InboundContext, NewConversation, NewMessage, PersonLookup};
use zupflow_core::channels::{ConnectionStatus, Direction};
use zupflow_core::ids::{ChannelConnectionId, ChannelMessageId, OrganizationId, RunId, WorkspaceId};
use zupflow_core::jobs::{Job, Retry};
use zupflow_core::keys::{self, EncryptionContext};
use zupflow_core::scope::{Name, Scope};

use crate::auth::AppState;
use crate::channels::OpenedConnection;

/// How long a claim holds a reply job: longer than a post and the read
/// that settles a lost one.
const LEASE: Duration = Duration::from_secs(90);

/// How long the reply worker waits when no job is due.
const IDLE: Duration = Duration::from_millis(250);

/// A connection's current token, opened: `Ok(None)` when it has none, or
/// what it holds can't be read (logged). `Err` with why when it's worth
/// trying again.
pub(crate) async fn connection_token(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    connection_id: ChannelConnectionId,
) -> Result<Option<Zeroizing<String>>, &'static str> {
    let Some(workspace) = state
        .workspaces
        .find_workspace(organization_id, workspace_id)
        .await
        .map_err(|_| "store_unavailable")?
    else {
        return Ok(None);
    };
    let Some(sealed) = state
        .channels
        .connection_credential(organization_id, workspace_id, connection_id)
        .await
        .map_err(|_| "store_unavailable")?
    else {
        return Ok(None);
    };
    let context = EncryptionContext::channel_connection_credential(
        organization_id,
        workspace_id,
        workspace.environment,
        connection_id,
        sealed.version,
    );
    let opened = keys::open(state.keys.as_ref(), &context, &sealed.sealed)
        .await
        .map_err(|_| "keys_unavailable")?;
    let Ok(mut connection) = serde_json::from_slice::<OpenedConnection>(opened.expose()) else {
        tracing::error!(event = "channels.token_unopenable", %organization_id, %workspace_id, %connection_id);
        return Ok(None);
    };
    Ok(Some(Zeroizing::new(std::mem::take(&mut connection.token))))
}

/// Reads from the provider the people a message names that its connection
/// has no fresh word of (see [`ChannelStore::people_to_read`]), at most
/// [`MAX_PEOPLE_READ`], and saves what it says, a person it doesn't know
/// included. `names` are read for their names: the author and the people
/// mentioned, once the message wakes an agent; `emails` for a fresh email:
/// the author, when a sender list is checked. The connection's own identity
/// is never read.
///
/// [`ChannelStore::people_to_read`]: zupflow_core::channels::store::ChannelStore::people_to_read
pub async fn learn_people(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    context: &InboundContext,
    names: &[String],
    emails: &[String],
) -> Learned {
    let unread = Learned {
        saved: false,
        unread: true,
    };
    let connection = &context.connection;
    if context.message.direction != Direction::Inbound || connection.status != ConnectionStatus::Connected {
        return Learned::default();
    }
    let own = [
        Some(connection.identity.external_id.as_str()),
        connection.identity.bot_id.as_deref(),
    ];
    let others = |ids: &[String]| -> Vec<String> {
        let mut kept: Vec<String> = Vec::new();
        for id in ids {
            if !own.contains(&Some(id.as_str())) && !kept.contains(id) {
                kept.push(id.clone());
            }
        }
        kept
    };
    let now = state.clock.now();
    let mut wanted = match state
        .channels
        .people_to_read(
            organization_id,
            workspace_id,
            connection.id,
            &others(names),
            &others(emails),
            now,
        )
        .await
    {
        Ok(wanted) if !wanted.is_empty() => wanted,
        Ok(_) => return Learned::default(),
        Err(error) => {
            tracing::warn!(event = "channels.people_unread", %organization_id, %workspace_id, connection_id = %connection.id, %error);
            return unread;
        }
    };
    wanted.truncate(MAX_PEOPLE_READ);
    let token = match connection_token(state, organization_id, workspace_id, connection.id).await {
        Ok(Some(token)) => token,
        // Nothing to read them with.
        Ok(None) => return Learned::default(),
        Err(why) => {
            tracing::warn!(event = "channels.people_unread", %organization_id, %workspace_id, connection_id = %connection.id, why);
            return unread;
        }
    };
    let provider = state.channel_providers.provider(connection.provider);
    let mut people = Vec::new();
    let mut failed = false;
    for id in wanted {
        match provider.person(state.outbound.as_ref(), &token, &id).await {
            Ok(found) => people.push(PersonLookup { external_id: id, found }),
            // A provider that can't say: the run reads their IDs.
            Err(ProviderCallError::Unsupported) => break,
            Err(error) => {
                // The rest wait: a run reads their IDs.
                tracing::warn!(event = "channels.people_unread", %organization_id, %workspace_id, connection_id = %connection.id, code = %error);
                failed = true;
                break;
            }
        }
    }
    if people.is_empty() {
        return Learned {
            saved: false,
            unread: failed,
        };
    }
    match state
        .channels
        .record_people(organization_id, workspace_id, connection.id, &people, now)
        .await
    {
        Ok(()) => Learned {
            saved: true,
            unread: failed,
        },
        Err(error) => {
            tracing::warn!(event = "channels.people_not_saved", %organization_id, %workspace_id, connection_id = %connection.id, %error);
            unread
        }
    }
}

/// What [`learn_people`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Learned {
    /// Something was saved: what was read of the connection's people
    /// before is out of date.
    pub saved: bool,
    /// Someone due to be read wasn't: the provider or the store failed.
    pub unread: bool,
}

/// What came of one try at a reply.
enum Next {
    /// Posted and recorded, or nothing to post.
    Done,
    /// Not posted, and trying again won't post it: logged with why.
    NotSent(&'static str),
    /// Try again, counting an attempt.
    Retry(&'static str),
    /// Wait, counting none: the provider asked to.
    Wait(Duration),
    /// Not posted, given up for good: logged with why, and buried.
    GiveUp(&'static str),
}

fn after_call(error: ProviderCallError) -> Next {
    match error {
        ProviderCallError::RateLimited { retry_after_secs } => Next::Wait(Duration::from_secs(retry_after_secs.max(1))),
        ProviderCallError::Unavailable => Next::Retry("provider_unavailable"),
        ProviderCallError::OutcomeUnknown => Next::Retry("outcome_unknown"),
        ProviderCallError::TokenRefused => Next::NotSent("token_refused"),
        ProviderCallError::Refused(code) => Next::NotSent(code),
        ProviderCallError::Unsupported => Next::NotSent("unsupported"),
    }
}

/// Records the post as the connection's outbound message, with its run and
/// hops: a post that meets its own echo, recorded first, fills it in.
async fn record_sent(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
    reply: &ChannelReply,
    external_id: String,
) -> Next {
    let now = state.clock.now();
    let recorded = state
        .channels
        .record_message(
            organization_id,
            workspace_id,
            reply.connection_id,
            &NewMessage {
                id: ChannelMessageId::generate(),
                conversation: NewConversation {
                    external_id: reply.conversation_external_id.clone(),
                    kind: reply.conversation_kind,
                    name: None,
                    is_external: reply.conversation_is_external,
                    identity_is_member: true,
                },
                direction: Direction::Outbound,
                author: None,
                external_id,
                thread_key: reply.thread_key.clone(),
                text: reply.text.clone(),
                mentions: Vec::new(),
                run_id: Some(run_id),
                tool_call_id: None,
                agent_hop: reply.agent_hop,
                sent_at: now,
            },
            now,
        )
        .await;
    match recorded {
        Ok(recorded) => {
            let connection_id = reply.connection_id;
            let message_id = recorded.message.id;
            tracing::info!(event = "channels.reply_sent", %organization_id, %workspace_id, %run_id, %connection_id, %message_id);
            Next::Done
        }
        // Posted: the next try finds it by the run's ID and records it.
        Err(_) => Next::Retry("store_unavailable"),
    }
}

/// Posts one run's answer, once.
async fn reply(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
    retried: bool,
) -> Next {
    let reply = match state.channels.reply_of(organization_id, workspace_id, run_id).await {
        Ok(Some(reply)) => reply,
        Ok(None) => return Next::Done,
        Err(_) => return Next::Retry("store_unavailable"),
    };
    if reply.sent.is_some() {
        return Next::Done;
    }
    match post_once(state, organization_id, workspace_id, run_id, &reply, retried).await {
        // A rate limit is waited out for a while after the run, not forever.
        Next::Wait(_) if reply_expired(reply.finished_at, state.clock.now()) => Next::GiveUp("rate_limited"),
        next => next,
    }
}

async fn post_once(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
    reply: &ChannelReply,
    retried: bool,
) -> Next {
    if reply.connection_status != ConnectionStatus::Connected {
        return Next::NotSent("connection_not_connected");
    }
    let token = match connection_token(state, organization_id, workspace_id, reply.connection_id).await {
        Ok(Some(token)) => token,
        Ok(None) => return Next::NotSent("no_token"),
        Err(why) => return Next::Retry(why),
    };
    let send_id = run_id.to_string();
    let post = OutgoingPost {
        conversation_external_id: &reply.conversation_external_id,
        thread_key: &reply.thread_key,
        text: &reply.text,
        send_id: &send_id,
    };
    let provider = state.channel_providers.provider(reply.provider);
    let http = state.outbound.as_ref();
    if retried {
        // An earlier try may have posted it and lost the answer: posted
        // again only once the provider says it wasn't.
        match provider.find_sent(http, &token, &post, reply.finished_at).await {
            Ok(Some(external_id)) => {
                return record_sent(state, organization_id, workspace_id, run_id, reply, external_id).await;
            }
            Ok(None) => {}
            // It can't look: posting again might post it twice.
            Err(ProviderCallError::Unsupported) => return Next::NotSent("outcome_unknown"),
            Err(error) => return after_call(error),
        }
    }
    match provider.post(http, &token, &post).await {
        Ok(external_id) => record_sent(state, organization_id, workspace_id, run_id, reply, external_id).await,
        Err(error) => after_call(error),
    }
}

/// Claims and handles one reply job, if one is due. Whether one was.
pub async fn work_once(state: &AppState) -> bool {
    let Ok(kind) = Name::new(REPLY_JOB) else {
        return false;
    };
    // Cross-tenant by design: a worker takes the next job of any scope.
    let job = match state.jobs.claim_next(std::slice::from_ref(&kind), LEASE).await {
        Ok(Some(job)) => job,
        Ok(None) => return false,
        Err(error) => {
            tracing::warn!(event = "channels.reply_claim_failed", %error);
            return false;
        }
    };
    handle(state, job).await;
    true
}

/// Handles one claimed reply job: completed, postponed, tried again, or
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
    let (Scope::Workspace(organization_id, workspace_id), Some(run_id)) = (scope, run_of_job(&payload)) else {
        tracing::error!(event = "channels.malformed_job", kind = REPLY_JOB);
        if let Ok(reason) = Name::new("malformed") {
            let _ = state.jobs.bury(&scope, id, &lease, &reason).await;
        }
        return;
    };
    let not_sent = |why: &str| {
        tracing::warn!(event = "channels.reply_not_sent", %organization_id, %workspace_id, %run_id, code = why, attempt);
    };
    let finished = match reply(state, organization_id, workspace_id, run_id, attempt > 1).await {
        Next::Done => state.jobs.complete(&scope, id, &lease).await,
        Next::NotSent(why) => {
            not_sent(why);
            state.jobs.complete(&scope, id, &lease).await
        }
        Next::Wait(after) => state.jobs.postpone(&scope, id, &lease, after).await,
        Next::GiveUp(why) => {
            not_sent(why);
            match Name::new(why) {
                Ok(reason) => state.jobs.bury(&scope, id, &lease, &reason).await,
                Err(_) => state.jobs.complete(&scope, id, &lease).await,
            }
        }
        Next::Retry(why) => {
            tracing::warn!(event = "channels.reply_retry", %organization_id, %workspace_id, %run_id, why, attempt);
            match Name::new(why) {
                Ok(reason) => {
                    let after = Duration::from_secs(5 * u64::from(attempt.max(1)));
                    let retried = state.jobs.retry(&scope, id, &lease, after, &reason).await;
                    if retried == Ok(Retry::Exhausted) {
                        not_sent(why);
                    }
                    retried.map(|_| ())
                }
                Err(_) => {
                    not_sent(why);
                    state.jobs.complete(&scope, id, &lease).await
                }
            }
        }
    };
    if let Err(error) = finished {
        tracing::warn!(event = "channels.reply_finish_failed", %organization_id, %workspace_id, %run_id, %error);
    }
}

/// Posts answers until the process stops: a loop of its own, so a slow
/// provider never holds up new messages or runs.
pub async fn run_worker(state: AppState) {
    loop {
        if !work_once(&state).await {
            tokio::time::sleep(IDLE).await;
        }
    }
}
