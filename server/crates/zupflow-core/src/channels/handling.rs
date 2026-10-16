//! Inbound handling: which agents answer a new message on a connection.
//!
//! Every new message goes to its connection's [`MessageHandler`], which only
//! decides which agents answer. The inbound job does everything around it,
//! before and after, so a handler can't skip a rule: it never sees an agent
//! the author isn't allowed to reach (the job filters candidates by the
//! sender rules, automated accounts, other agents and the hop limit), and it
//! can't start a run itself (the job records each dispatch, counts the
//! conversation's breaker and starts the run).
//!
//! [`MentionHandler`] is the only handler built: a DM, a mention, or a
//! follow-up in a thread the agent was addressed in. A later handler is a
//! second implementation of the trait, with no change to the job or the store.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::provider::JoinedConversation;
use super::store::ChannelSessionKey;
use super::{ConversationKind, HandlerKind, ThreadFollowUp};
use crate::error::ApiError;
use crate::ids::{AgentChannelId, AgentId, ChannelAppId, ChannelConnectionId, ChannelMessageId, ChannelPersonId};
use crate::jobs::JobOptions;
use crate::runs::QueuedJob;
use crate::scope::Name;
use crate::time::Timestamp;

/// The kind of the job a new message queues, in the transaction that
/// records it, for the inbound job to decide who answers it.
pub const INBOUND_JOB: &str = "channel_inbound";

/// How many times the inbound job is tried. Each step is idempotent: a
/// message handled before stops at once, and a dispatch is unique per agent
/// and message.
pub const INBOUND_JOB_ATTEMPTS: u32 = 5;

/// The inbound job for a new message: its payload names the message only.
pub fn inbound_job(message_id: ChannelMessageId) -> Option<QueuedJob> {
    Some(QueuedJob {
        kind: Name::new(INBOUND_JOB).ok()?,
        payload: json!({ "message_id": message_id }).to_string().into_bytes(),
        options: JobOptions {
            delay: Duration::ZERO,
            max_attempts: INBOUND_JOB_ATTEMPTS,
        },
    })
}

/// The message an inbound job's payload names.
pub fn message_of_job(payload: &[u8]) -> Option<ChannelMessageId> {
    let payload: Value = serde_json::from_slice(payload).ok()?;
    ChannelMessageId::parse(payload.get("message_id")?.as_str()?).ok()
}

/// The kind of the job a connection's own join queues, in the transaction
/// that records it: a provider tells an app nothing of a conversation until
/// it's in it, so the mention that got it added there would go unheard.
/// The job reads what was said there just before the join
/// ([`CATCH_UP_WINDOW`], [`CATCH_UP_LIMIT`]) and records each message that
/// mentions the connection as a delivery would, which queues its inbound
/// job; a message recorded before is never queued again, so nothing is
/// answered twice.
pub const CATCH_UP_JOB: &str = "channel_catch_up";

/// How far before its join a connection looks back.
pub const CATCH_UP_WINDOW: Duration = Duration::from_secs(10 * 60);

/// How many messages it reads, at most: one page, within the least a
/// provider's history call gives an app.
pub const CATCH_UP_LIMIT: u32 = 15;

/// How many times the catch-up job is tried; a rate limit waits without
/// using one.
pub const CATCH_UP_JOB_ATTEMPTS: u32 = 5;

/// What a catch-up job names: references only, never a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchUp {
    pub app_id: ChannelAppId,
    pub connection_id: ChannelConnectionId,
    pub conversation: JoinedConversation,
}

/// The catch-up job for a connection's join.
pub fn catch_up_job(
    app_id: ChannelAppId,
    connection_id: ChannelConnectionId,
    joined: &JoinedConversation,
) -> Option<QueuedJob> {
    Some(QueuedJob {
        kind: Name::new(CATCH_UP_JOB).ok()?,
        payload: json!({
            "app_id": app_id,
            "connection_id": connection_id,
            "conversation": joined.conversation_external_id,
            "conversation_kind": joined.conversation_kind.as_str(),
            "conversation_is_external": joined.conversation_is_external,
            "member": joined.member_external_id,
            "joined_at": joined.joined_at.millis(),
        })
        .to_string()
        .into_bytes(),
        options: JobOptions {
            delay: Duration::ZERO,
            max_attempts: CATCH_UP_JOB_ATTEMPTS,
        },
    })
}

/// What a catch-up job's payload names.
pub fn catch_up_of_job(payload: &[u8]) -> Option<CatchUp> {
    let payload: Value = serde_json::from_slice(payload).ok()?;
    let text = |name: &str| payload.get(name).and_then(Value::as_str);
    Some(CatchUp {
        app_id: ChannelAppId::parse(text("app_id")?).ok()?,
        connection_id: ChannelConnectionId::parse(text("connection_id")?).ok()?,
        conversation: JoinedConversation {
            conversation_external_id: text("conversation")?.to_owned(),
            conversation_kind: ConversationKind::parse(text("conversation_kind")?)?,
            conversation_is_external: payload.get("conversation_is_external")?.as_bool()?,
            member_external_id: text("member")?.to_owned(),
            joined_at: Timestamp::from_millis(payload.get("joined_at")?.as_i64()?),
        },
    })
}

/// Decides which agents answer a new inbound message. It proposes; the
/// inbound job enforces every rule before and after it.
#[async_trait]
pub trait MessageHandler: Send + Sync {
    fn kind(&self) -> HandlerKind;

    /// Pure for [`MentionHandler`]. A handler that calls a model would do it
    /// here, inside the job, never inside the provider's acknowledgement.
    async fn decide(&self, input: &HandlerInput<'_>) -> Result<Vec<Dispatch>, ApiError>;
}

/// What a handler decides from. The scope is the job's, from the app the
/// event was delivered to; the candidates are already filtered.
#[derive(Debug, Clone)]
pub struct HandlerInput<'a> {
    pub message: &'a InboundMessage,
    pub thread: &'a ThreadState,
    /// Agents that may answer this author here, already filtered by the job.
    pub candidates: &'a [Candidate],
}

/// A new message, as the provider normalized it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundMessage {
    pub id: ChannelMessageId,
    /// The provider's key for the message across connections (team, conversation, time).
    pub origin_key: String,
    pub thread_key: String,
    pub author: Author,
    /// A DM to this connection's identity.
    pub is_direct: bool,
    /// The provider's IDs of who it mentions, in order.
    pub mentions: Vec<String>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Author {
    Person(ChannelPersonId),
    /// An app or integration posting automatically.
    Automated(ChannelPersonId),
    /// Another agent of the workspace, posting through its own connection,
    /// with the hops that led to its message.
    Agent {
        agent_channel_id: AgentChannelId,
        hop: u32,
    },
}

/// What the job knows of the thread across every agent of the workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadState {
    /// The agent most recently woken by a DM or a mention in this thread.
    pub last_addressed: Option<AgentChannelId>,
}

/// An agent that may answer: its channel on the connection and what it answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub agent_channel_id: AgentChannelId,
    pub agent_id: AgentId,
    /// The provider's ID of the identity this agent answers as.
    pub identity: String,
    pub answers_dms: bool,
    pub answers_mentions: bool,
    pub thread_follow_up: ThreadFollowUp,
    /// Its session in this thread, when it's already answering in it.
    pub in_thread: Option<ChannelSessionKey>,
}

/// One agent the handler proposes, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dispatch {
    pub agent_channel_id: AgentChannelId,
    pub reason: DispatchReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchReason {
    DirectMessage,
    Mentioned,
    ThreadFollowUp,
}

impl DispatchReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DirectMessage => "direct_message",
            Self::Mentioned => "mentioned",
            Self::ThreadFollowUp => "thread_follow_up",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "direct_message" => Some(Self::DirectMessage),
            "mentioned" => Some(Self::Mentioned),
            "thread_follow_up" => Some(Self::ThreadFollowUp),
            _ => None,
        }
    }

    /// Whether the agent was addressed: a DM or a mention, which make it the
    /// thread's last addressed agent.
    pub fn addresses(self) -> bool {
        matches!(self, Self::DirectMessage | Self::Mentioned)
    }
}

/// Wakes an agent on a DM to its identity, a mention of it, or a reply in a
/// thread it's answering in that mentions nobody: with the default, only
/// when it was the last agent addressed there.
#[derive(Debug, Clone, Copy, Default)]
pub struct MentionHandler;

impl MentionHandler {
    /// The decision itself, with nothing to wait for.
    pub fn decide_now(input: &HandlerInput<'_>) -> Vec<Dispatch> {
        let message = input.message;
        input
            .candidates
            .iter()
            .filter_map(|candidate| {
                let follows = candidate.in_thread.is_some()
                    && message.mentions.is_empty()
                    && match candidate.thread_follow_up {
                        ThreadFollowUp::Off => false,
                        ThreadFollowUp::WhenLastAddressed => {
                            input.thread.last_addressed == Some(candidate.agent_channel_id)
                        }
                        ThreadFollowUp::Always => true,
                    };
                let reason = if message.is_direct && candidate.answers_dms {
                    DispatchReason::DirectMessage
                } else if candidate.answers_mentions && message.mentions.contains(&candidate.identity) {
                    DispatchReason::Mentioned
                } else if follows {
                    DispatchReason::ThreadFollowUp
                } else {
                    return None;
                };
                Some(Dispatch {
                    agent_channel_id: candidate.agent_channel_id,
                    reason,
                })
            })
            .collect()
    }
}

#[async_trait]
impl MessageHandler for MentionHandler {
    fn kind(&self) -> HandlerKind {
        HandlerKind::Mention
    }

    async fn decide(&self, input: &HandlerInput<'_>) -> Result<Vec<Dispatch>, ApiError> {
        Ok(Self::decide_now(input))
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    fn candidate(identity: &str, follow_up: ThreadFollowUp, in_thread: bool) -> Candidate {
        Candidate {
            agent_channel_id: AgentChannelId::generate(),
            agent_id: AgentId::generate(),
            identity: identity.to_owned(),
            answers_dms: true,
            answers_mentions: true,
            thread_follow_up: follow_up,
            in_thread: in_thread.then(|| ChannelSessionKey::from_uuid(Uuid::now_v7())),
        }
    }

    fn message(is_direct: bool, mentions: &[&str]) -> InboundMessage {
        InboundMessage {
            id: ChannelMessageId::generate(),
            origin_key: "T1/C1/1.000001".to_owned(),
            thread_key: "1.000000".to_owned(),
            author: Author::Person(ChannelPersonId::generate()),
            is_direct,
            mentions: mentions.iter().map(|m| (*m).to_owned()).collect(),
            text: "hello".to_owned(),
        }
    }

    fn decide(message: &InboundMessage, thread: &ThreadState, candidates: &[Candidate]) -> Vec<Dispatch> {
        MentionHandler::decide_now(&HandlerInput {
            message,
            thread,
            candidates,
        })
    }

    #[test]
    fn answers_a_direct_message_when_the_agent_answers_dms() {
        let mut agent = candidate("U1", ThreadFollowUp::WhenLastAddressed, false);
        let dm = message(true, &[]);
        assert_eq!(
            decide(&dm, &ThreadState::default(), std::slice::from_ref(&agent)),
            vec![Dispatch {
                agent_channel_id: agent.agent_channel_id,
                reason: DispatchReason::DirectMessage
            }]
        );
        agent.answers_dms = false;
        assert!(decide(&dm, &ThreadState::default(), &[agent]).is_empty());
    }

    #[test]
    fn answers_a_mention_of_its_own_identity_only() {
        let billing = candidate("U1", ThreadFollowUp::WhenLastAddressed, false);
        let support = candidate("U2", ThreadFollowUp::WhenLastAddressed, false);
        let both = [billing.clone(), support.clone()];
        assert_eq!(
            decide(&message(false, &["U1"]), &ThreadState::default(), &both),
            vec![Dispatch {
                agent_channel_id: billing.agent_channel_id,
                reason: DispatchReason::Mentioned
            }]
        );
        assert_eq!(
            decide(&message(false, &["U2", "U1"]), &ThreadState::default(), &both).len(),
            2
        );
        assert!(decide(&message(false, &["U9"]), &ThreadState::default(), &both).is_empty());
        let mut quiet = billing;
        quiet.answers_mentions = false;
        assert!(decide(&message(false, &["U1"]), &ThreadState::default(), &[quiet]).is_empty());
    }

    #[test]
    fn follows_up_in_a_thread_only_as_set() {
        let last = candidate("U1", ThreadFollowUp::WhenLastAddressed, true);
        let earlier = candidate("U2", ThreadFollowUp::WhenLastAddressed, true);
        let always = candidate("U3", ThreadFollowUp::Always, true);
        let never = candidate("U4", ThreadFollowUp::Off, true);
        let outside = candidate("U5", ThreadFollowUp::Always, false);
        let thread = ThreadState {
            last_addressed: Some(last.agent_channel_id),
        };
        let all = [last.clone(), earlier, always.clone(), never, outside];
        let woken: Vec<AgentChannelId> = decide(&message(false, &[]), &thread, &all)
            .into_iter()
            .inspect(|dispatch| assert_eq!(dispatch.reason, DispatchReason::ThreadFollowUp))
            .map(|dispatch| dispatch.agent_channel_id)
            .collect();
        assert_eq!(woken, vec![last.agent_channel_id, always.agent_channel_id]);
        // A reply addressed to someone else wakes no one else.
        assert!(decide(&message(false, &["U9"]), &thread, &all).is_empty());
    }

    #[test]
    fn queues_a_job_naming_the_message_only() {
        let id = ChannelMessageId::generate();
        let job = inbound_job(id).expect("a job");
        assert_eq!(job.kind.as_str(), INBOUND_JOB);
        assert_eq!(message_of_job(&job.payload), Some(id));
        assert_eq!(message_of_job(b"{}"), None);
    }

    #[test]
    fn names_a_joined_conversation_in_its_catch_up_job() {
        let app_id = ChannelAppId::generate();
        let connection_id = ChannelConnectionId::generate();
        let joined = JoinedConversation {
            conversation_external_id: "C0GENERAL".to_owned(),
            conversation_kind: ConversationKind::PrivateChannel,
            conversation_is_external: true,
            member_external_id: "U0BOT".to_owned(),
            joined_at: Timestamp::from_millis(1_760_000_000_123),
        };
        let job = catch_up_job(app_id, connection_id, &joined).expect("its kind is a valid name");
        assert_eq!(job.kind.as_str(), CATCH_UP_JOB);
        assert_eq!(
            catch_up_of_job(&job.payload),
            Some(CatchUp {
                app_id,
                connection_id,
                conversation: joined
            })
        );
        assert_eq!(catch_up_of_job(b"{}"), None);
    }

    #[test]
    fn is_the_mention_handler() {
        assert_eq!(MentionHandler.kind(), HandlerKind::Mention);
        assert_eq!(DispatchReason::ThreadFollowUp.as_str(), "thread_follow_up");
    }
}
