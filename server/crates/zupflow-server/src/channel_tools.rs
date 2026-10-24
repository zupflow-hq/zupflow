//! A channel run's tools (decision 0047's names): what its session offers,
//! what decides on each call, the checks again before one is made, and the
//! calls themselves, through the run's own connection with its own token.
//!
//! **Scope.** Everything a call names is looked up in the run's
//! organization and workspace, and within its own connection: a
//! conversation, message or person of another connection, even one of the
//! same workspace, is the same as one that doesn't exist. The provider is
//! only ever asked with the run's connection's token.
//!
//! **Rules.** Every call that changes something passes, in order: the
//! audience rule (`outside_audience`: the agent posts only where its agent
//! channel lets it, and only in conversations it's in; in a conversation
//! shared with another organization other than where the run began, or to
//! someone of another team, only when its agent channel allows outsiders;
//! and edits or deletes only its own messages), the rate limits (`rate_limited`: one change a second per
//! connection, reserved in turn and waited out, and 200 an hour; 20 a run,
//! counting posts, edits, reactions and deletions alike), the grant and the
//! organization's level, then approval when either asks for it. The rules are read again just before the call is
//! made, and the call's digest (the connection, the tool, the exact
//! arguments and where the run began) is checked against what was decided
//! and approved.
//!
//! **Reads.** What a read shows is rendered as the run's turns are: the
//! provider's markup in plain words, with the connection's names, so no
//! provider ID reaches the model. A direct message is read only in the
//! conversation the run began in.
//!
//! **Posts.** A post carries the call's ID ([`OutgoingPost::send_id`]): a
//! post whose answer was lost is looked for, and never made again in the
//! same breath (not found, its outcome is unknown); one whose sender stopped
//! is settled by finding it ([`settle_lapsed`]). Each post is
//! recorded as the connection's outbound message, with the run, the call
//! and one more hop than the messages that started the run, and announced
//! (`channel_message.sent`).
//!
//! **Typing.** While a channel run asks its model, the thread shows that the
//! agent is working ([`Working`]): best effort, refreshed every 10 seconds,
//! never retried, and never a reason for a run to fail.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};
use zeroize::Zeroizing;
use zupflow_core::channels::provider::{ChannelProvider, MessageRef, OutgoingPost, ProviderCallError};
use zupflow_core::channels::store::{
    ConversationFilter, KnownPerson, MessageFilter, NewConversation, NewMessage, RunChannelContext, SENDS_PER_HOUR,
    SendSlot,
};
use zupflow_core::channels::thread::{MentionNames, conversation_mentions, plain_text};
use zupflow_core::channels::tools::{self, MAX_FOUND, MAX_POST, MAX_READ, OfferingConnection};
use zupflow_core::channels::{
    AgentChannel, Audience, ChannelConnection, ChannelConversation, ChannelMessage, ConnectionStatus, ConversationKind,
    Direction,
};
use zupflow_core::ids::{
    ChannelConnectionId, ChannelConversationId, ChannelMessageId, ChannelPersonId, OrganizationId, RunId, ToolCallId,
    WorkspaceId,
};
use zupflow_core::outbound::OutboundHttp;
use zupflow_core::policy::{Rule, ToolLevel};
use zupflow_core::runs::digest::channel_call_digest;
use zupflow_core::runs::names::OfferedTool;
use zupflow_core::runs::reducer::{
    CONNECTION_REMOVED, CONNECTION_UNUSABLE, ChannelSendRequest, InstallationCheck, SendCheck,
};
use zupflow_core::runs::state::{CallDecision, CallRecord, CallVerdict, ChannelOrigin};
use zupflow_core::runs::transition::SendOutcome;
use zupflow_core::time::Timestamp;

use crate::auth::AppState;
use crate::channel_reply::connection_token;

/// The policy a channel call's decision names.
pub const POLICY: &str = "channel";

/// The rule, and the refusal's code, for a call outside the audience.
pub const OUTSIDE_AUDIENCE: &str = "outside_audience";

/// The rule, and the refusal's code, for a call over a rate limit.
pub const RATE_LIMITED: &str = "rate_limited";

/// A call whose arguments can't be acted on.
const INVALID_ARGUMENTS: &str = "invalid_arguments";

/// The most changes one run makes at its provider: posts, edits, reactions
/// and deletions alike.
pub const MAX_CHANGES_PER_RUN: u32 = 20;

/// The most messages a search reads before matching their rendered text.
const SEARCH_CANDIDATES: u32 = 500;

/// How often the thread is told again that the agent is working.
const WORKING_EVERY: Duration = Duration::from_secs(10);

/// Why a call can't be made, for the model and the timeline.
struct Refusal {
    rule: &'static str,
    message: String,
}

fn refusal(rule: &'static str, message: impl Into<String>) -> Refusal {
    Refusal {
        rule,
        message: message.into(),
    }
}

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

// --- What a session offers ----------------------------------------------------

/// The channel tools a session in `channel` offers through `connection`,
/// and `stay_silent`: read when the session starts, and fixed for it.
pub(crate) async fn session_tools(
    state: &AppState,
    organization_id: OrganizationId,
    connection: &ChannelConnection,
    channel: &AgentChannel,
) -> Result<Vec<OfferedTool>, &'static str> {
    let levels = state
        .channels
        .list_tool_levels(organization_id)
        .await
        .map_err(|_| "store_unavailable")?;
    let offering = OfferingConnection {
        id: connection.id,
        provider: connection.provider,
        short_id: connection.short_id.as_str(),
        label: &connection.label,
        handle: &connection.identity.handle,
    };
    let level = |tool: &str| {
        levels
            .iter()
            .find(|level| level.provider == connection.provider && level.tool == tool)
            .map_or(ToolLevel::Deny, |level| level.level)
    };
    let mut offered = tools::offered(&offering, &channel.settings.tools, &level);
    offered.extend(tools::stay_silent());
    Ok(offered)
}

// --- Resolving what a call names ------------------------------------------------

/// What a call that changes something acts on, found within the run's
/// connection.
enum Target {
    /// A post in a conversation: at its top level when `thread_key` is empty.
    Post {
        conversation: ChannelConversation,
        thread_key: String,
        text: String,
    },
    /// A direct message to someone.
    Direct { person: KnownPerson, text: String },
    /// An action on one message; boxed, as it's large beside the others.
    Message {
        conversation: ChannelConversation,
        message: Box<ChannelMessage>,
        action: Action,
    },
}

enum Action {
    React { emoji: String, remove: bool },
    Edit { text: String },
    Delete,
}

type Resolved<T> = Result<Result<T, Refusal>, &'static str>;

fn string<'a>(arguments: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// The text a post or an edit takes: there, and not longer than
/// [`MAX_POST`].
fn text_of(arguments: &Map<String, Value>) -> Result<String, Refusal> {
    let Some(text) = string(arguments, "text") else {
        return Err(refusal(INVALID_ARGUMENTS, "Give the message's text."));
    };
    if text.chars().count() > MAX_POST {
        return Err(refusal(
            INVALID_ARGUMENTS,
            format!("The text is longer than {MAX_POST} characters. Shorten it, or split it into messages."),
        ));
    }
    Ok(text.to_owned())
}

/// Whether the run may post in, or act on a message of, `conversation`:
/// where it began, or, as its agent channel's audience allows, a listed
/// conversation or anywhere, and only where its identity is a member.
fn within(run: &RunChannelContext, conversation: &ChannelConversation) -> bool {
    if conversation.id == run.origin.id {
        return true;
    }
    let settings = &run.agent_channel.settings;
    conversation.identity_is_member
        && match settings.audience {
            Audience::Origin => false,
            Audience::Listed => settings.audience_conversations.contains(&conversation.id),
            Audience::Anywhere => true,
        }
}

/// A conversation shared with another organization takes the agent's
/// changes only when its agent channel allows outsiders, or when the run
/// began there: its answer goes there whichever way it's posted.
fn shared_refusal(run: &RunChannelContext, conversation: &ChannelConversation) -> Option<Refusal> {
    let shared = conversation.is_external && conversation.id != run.origin.id;
    (shared && !run.agent_channel.settings.allow_external).then(|| {
        refusal(
            OUTSIDE_AUDIENCE,
            "That conversation is shared with another organization, and this agent doesn't act in shared \
             conversations.",
        )
    })
}

/// The audience rule for a change in `conversation`: `None` when it may be
/// made.
fn audience_refusal(run: &RunChannelContext, conversation: &ChannelConversation) -> Option<Refusal> {
    if !within(run, conversation) {
        return Some(outside(run));
    }
    shared_refusal(run, conversation)
}

/// Whether a read may show `conversation`: a direct message only when the
/// run began in it.
fn readable(run: &RunChannelContext, conversation: &ChannelConversation) -> bool {
    !matches!(conversation.kind, ConversationKind::Dm | ConversationKind::GroupDm) || conversation.id == run.origin.id
}

fn outside(run: &RunChannelContext) -> Refusal {
    let provider = run.connection.provider.name();
    refusal(
        OUTSIDE_AUDIENCE,
        match run.agent_channel.settings.audience {
            Audience::Origin => {
                format!("You can only post in the {provider} conversation this run started in. Answer there instead.")
            }
            Audience::Listed => format!(
                "You can only post in the {provider} conversation this run started in and the ones listed for you."
            ),
            Audience::Anywhere => format!("You can only post in {provider} conversations you're a member of."),
        },
    )
}

async fn conversation_named(
    state: &AppState,
    run: &RunChannelContext,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    named: Option<&str>,
) -> Resolved<ChannelConversation> {
    let unknown = || {
        Ok(Err(refusal(
            OUTSIDE_AUDIENCE,
            "That conversation isn't one this connection knows. Use an ID list_conversations gives.",
        )))
    };
    let Some(id) = named.and_then(|named| ChannelConversationId::parse(named).ok()) else {
        return unknown();
    };
    match state
        .channels
        .find_conversation(organization_id, workspace_id, run.connection.id, id)
        .await
    {
        Ok(Some(conversation)) => Ok(Ok(conversation)),
        Ok(None) => unknown(),
        Err(_) => Err("store_unavailable"),
    }
}

/// One of the connection's messages, with its conversation.
async fn message_named(
    state: &AppState,
    run: &RunChannelContext,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    named: Option<&str>,
) -> Resolved<(ChannelConversation, ChannelMessage)> {
    let unknown = || {
        Ok(Err(refusal(
            OUTSIDE_AUDIENCE,
            "That message isn't one this connection knows. Use an ID a read gives.",
        )))
    };
    let Some(id) = named.and_then(|named| ChannelMessageId::parse(named).ok()) else {
        return unknown();
    };
    let Some(message) = state
        .channels
        .find_message(organization_id, workspace_id, run.connection.id, id)
        .await
        .map_err(|_| "store_unavailable")?
    else {
        return unknown();
    };
    let Some(conversation) = state
        .channels
        .find_conversation(
            organization_id,
            workspace_id,
            run.connection.id,
            message.conversation_id,
        )
        .await
        .map_err(|_| "store_unavailable")?
    else {
        return unknown();
    };
    Ok(Ok((conversation, message)))
}

/// What a call that changes something acts on, held to the audience rule;
/// `None` for a tool that only reads.
async fn resolve(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run: &RunChannelContext,
    tool: &str,
    arguments: &Map<String, Value>,
) -> Resolved<Option<Target>> {
    macro_rules! found {
        ($resolved:expr) => {
            match $resolved? {
                Ok(found) => found,
                Err(refused) => return Ok(Err(refused)),
            }
        };
    }
    macro_rules! valid {
        ($checked:expr) => {
            match $checked {
                Ok(found) => found,
                Err(refused) => return Ok(Err(refused)),
            }
        };
    }
    let target = match tool {
        "reply" => Target::Post {
            conversation: run.origin.clone(),
            thread_key: run.thread_key.clone(),
            text: valid!(text_of(arguments)),
        },
        "send_message" => {
            let text = valid!(text_of(arguments));
            let conversation = found!(
                conversation_named(
                    state,
                    run,
                    organization_id,
                    workspace_id,
                    string(arguments, "conversation")
                )
                .await
            );
            if let Some(refused) = audience_refusal(run, &conversation) {
                return Ok(Err(refused));
            }
            let thread_key = match string(arguments, "thread") {
                None => String::new(),
                Some(thread) => {
                    let (in_conversation, message) =
                        found!(message_named(state, run, organization_id, workspace_id, Some(thread)).await);
                    if in_conversation.id != conversation.id {
                        return Ok(Err(refusal(
                            INVALID_ARGUMENTS,
                            "That thread's message is in another conversation. Name a message in this one.",
                        )));
                    }
                    if message.thread_key.is_empty() {
                        message.external_id
                    } else {
                        message.thread_key
                    }
                }
            };
            Target::Post {
                conversation,
                thread_key,
                text,
            }
        }
        "send_direct_message" => {
            let text = valid!(text_of(arguments));
            let person = match string(arguments, "person").and_then(|named| ChannelPersonId::parse(named).ok()) {
                Some(id) => state
                    .channels
                    .find_person(organization_id, workspace_id, run.connection.id, id)
                    .await
                    .map_err(|_| "store_unavailable")?,
                None => None,
            };
            let Some(person) = person else {
                return Ok(Err(refusal(
                    OUTSIDE_AUDIENCE,
                    "That person isn't one this connection knows. Use an ID find_people gives.",
                )));
            };
            // A direct message opens a conversation of its own: only an
            // agent channel that may post anywhere sends one.
            if run.agent_channel.settings.audience != Audience::Anywhere {
                return Ok(Err(refusal(
                    OUTSIDE_AUDIENCE,
                    format!(
                        "You can't start direct messages in {}. Answer in the conversation this run started in.",
                        run.connection.provider.name()
                    ),
                )));
            }
            if person.is_automated {
                return Ok(Err(refusal(
                    INVALID_ARGUMENTS,
                    "That's an automated account, not a person.",
                )));
            }
            // Someone of another team, or of a team no message of theirs
            // named, is outside the organization's own.
            let own_team = person.team_id.as_deref() == Some(run.connection.team.id.as_str());
            if !own_team && !run.agent_channel.settings.allow_external {
                return Ok(Err(refusal(
                    OUTSIDE_AUDIENCE,
                    "That person isn't in your team, and this agent doesn't message people outside it.",
                )));
            }
            Target::Direct { person, text }
        }
        "react" | "edit_message" | "delete_message" => {
            let action = match tool {
                "react" => {
                    let Some(emoji) = string(arguments, "emoji") else {
                        return Ok(Err(refusal(INVALID_ARGUMENTS, "Give the emoji's name.")));
                    };
                    let emoji = emoji.trim_matches(':');
                    if emoji.is_empty()
                        || emoji.len() > 100
                        || !emoji.chars().all(|c| c.is_ascii_alphanumeric() || "_-+'".contains(c))
                    {
                        return Ok(Err(refusal(INVALID_ARGUMENTS, "Give the emoji's name, such as eyes.")));
                    }
                    Action::React {
                        emoji: emoji.to_owned(),
                        remove: arguments.get("remove").and_then(Value::as_bool).unwrap_or(false),
                    }
                }
                "edit_message" => Action::Edit {
                    text: valid!(text_of(arguments)),
                },
                _ => Action::Delete,
            };
            let (conversation, message) =
                found!(message_named(state, run, organization_id, workspace_id, string(arguments, "message")).await);
            if let Some(refused) = audience_refusal(run, &conversation) {
                return Ok(Err(refused));
            }
            if message.deleted_at.is_some() {
                return Ok(Err(refusal(INVALID_ARGUMENTS, "That message was deleted.")));
            }
            let own = message.direction == Direction::Outbound;
            if !own && !matches!(action, Action::React { .. }) {
                return Ok(Err(refusal(
                    OUTSIDE_AUDIENCE,
                    "You can only edit or delete messages you posted.",
                )));
            }
            Target::Message {
                conversation,
                message: Box::new(message),
                action,
            }
        }
        _ => return Ok(Ok(None)),
    };
    Ok(Ok(Some(target)))
}

// --- Decisions ---------------------------------------------------------------

/// The rules' decision on a call to a channel tool, read now: the audience,
/// the rate limits (the run's own only when `earlier_changes` counts what
/// it changed before), the grant and the organization's level, then
/// approval.
pub(crate) async fn decide(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
    tool: &str,
    arguments: &Map<String, Value>,
    earlier_changes: Option<u32>,
) -> Result<CallDecision, &'static str> {
    let Some(run) = state
        .channels
        .run_channel(organization_id, workspace_id, run_id)
        .await
        .map_err(|_| "store_unavailable")?
    else {
        return Ok(deny(
            Rule::NotGranted.as_str(),
            "This run's channel connection is gone, so its tools can't be used.",
        ));
    };
    decide_for(
        state,
        organization_id,
        workspace_id,
        &run,
        tool,
        arguments,
        earlier_changes,
    )
    .await
}

async fn decide_for(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run: &RunChannelContext,
    tool: &str,
    arguments: &Map<String, Value>,
    earlier_changes: Option<u32>,
) -> Result<CallDecision, &'static str> {
    if tools::writes(tool)
        && let Err(refused) = resolve(state, organization_id, workspace_id, run, tool, arguments).await?
    {
        return Ok(deny(refused.rule, refused.message));
    }
    if tools::writes(tool) {
        if earlier_changes.is_some_and(|changes| changes >= MAX_CHANGES_PER_RUN) {
            return Ok(deny(
                RATE_LIMITED,
                format!("A run makes at most {MAX_CHANGES_PER_RUN} posts, edits and reactions, and this one has."),
            ));
        }
        let counts = state
            .channels
            .send_counts(organization_id, workspace_id, run.connection.id, state.clock.now())
            .await
            .map_err(|_| "store_unavailable")?;
        if counts.hour >= SENDS_PER_HOUR {
            return Ok(deny(RATE_LIMITED, hour_full(run)));
        }
    }
    let settings = &run.agent_channel.settings;
    if !settings.tools.iter().any(|granted| granted == tool) {
        return Ok(deny(
            Rule::NotGranted.as_str(),
            format!("{tool} isn't granted to this agent."),
        ));
    }
    let level = state
        .channels
        .list_tool_levels(organization_id)
        .await
        .map_err(|_| "store_unavailable")?
        .into_iter()
        .find(|level| level.provider == run.connection.provider && level.tool == tool)
        .map_or(ToolLevel::Deny, |level| level.level);
    Ok(match level {
        ToolLevel::Deny => deny(
            Rule::OrganizationDenies.as_str(),
            format!("Your organization doesn't allow {tool}."),
        ),
        ToolLevel::Approve => decision(
            CallVerdict::RequireApproval {
                reason: format!("Your organization requires approval for {tool}."),
            },
            Rule::OrganizationRequiresApproval.as_str(),
        ),
        ToolLevel::Allow if settings.approval_required.iter().any(|named| named == tool) => decision(
            CallVerdict::RequireApproval {
                reason: format!("This agent needs approval for {tool}."),
            },
            Rule::GrantRequiresApproval.as_str(),
        ),
        ToolLevel::Allow => decision(CallVerdict::Allow, Rule::Allowed.as_str()),
    })
}

/// A channel call checked again just before it's sent, or, waiting for
/// approval, before the run parks on it: then only whether its connection
/// is still there counts, and its decision stays the one recorded.
pub(crate) async fn send_check(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
    call: &CallRecord,
    parking: bool,
) -> Result<SendCheck, &'static str> {
    let (Some(target), Some(recorded)) = (&call.target, &call.decision) else {
        return Err("run_inconsistent");
    };
    let Some(connection_id) = target.channel() else {
        return Err("run_inconsistent");
    };
    let connection = state
        .channels
        .find_connection(organization_id, workspace_id, connection_id)
        .await
        .map_err(|_| "store_unavailable")?;
    let check = match connection.map(|connection| connection.status) {
        None => InstallationCheck::Removed,
        Some(ConnectionStatus::Disconnected) => InstallationCheck::Disconnected,
        Some(ConnectionStatus::ReconnectRequired) if !parking => InstallationCheck::Unusable,
        Some(_) => InstallationCheck::Usable,
    };
    if parking {
        return Ok(SendCheck {
            call_id: call.id,
            installation: check,
            definition_sha256: Some(target.definition_sha256),
            decision: recorded.clone(),
        });
    }
    let decision = decide(
        state,
        organization_id,
        workspace_id,
        run_id,
        &target.tool,
        &call.arguments,
        None,
    )
    .await?;
    Ok(SendCheck {
        call_id: call.id,
        installation: check,
        definition_sha256: tools::definition_sha256(&target.tool),
        decision,
    })
}

// --- Making calls ---------------------------------------------------------------

fn hour_full(run: &RunChannelContext) -> String {
    format!(
        "{} made {SENDS_PER_HOUR} posts, edits and reactions in the last hour, the most it may. Try again later.",
        run.connection.label
    )
}

fn refused(code: &str, message: impl Into<String>) -> SendOutcome {
    SendOutcome::Refused {
        code: code.to_owned(),
        message: message.into(),
    }
}

fn completed(result: Value) -> SendOutcome {
    match result {
        Value::Object(result) => SendOutcome::Completed { result },
        other => {
            let mut result = Map::new();
            result.insert("result".to_owned(), other);
            SendOutcome::Completed { result }
        }
    }
}

fn tool_error(message: &str) -> SendOutcome {
    let mut result = Map::new();
    result.insert("error".to_owned(), json!(message));
    SendOutcome::ToolError { result }
}

/// A provider's refusal as the call's outcome: never the provider's words.
fn provider_refusal(run: &RunChannelContext, error: ProviderCallError) -> SendOutcome {
    let provider = run.connection.provider.name();
    match error {
        ProviderCallError::RateLimited { retry_after_secs } => refused(
            RATE_LIMITED,
            format!("{provider} asked to slow down: try again in {retry_after_secs} seconds."),
        ),
        ProviderCallError::TokenRefused => refused(
            CONNECTION_UNUSABLE,
            format!(
                "{} can't reach {provider} until it's reconnected, so this call wasn't made.",
                run.connection.label
            ),
        ),
        ProviderCallError::Unavailable => refused(
            "channel_unavailable",
            format!("{provider} couldn't be reached, so this call wasn't made. Try again."),
        ),
        ProviderCallError::OutcomeUnknown => SendOutcome::Unknown,
        ProviderCallError::Refused(code) => refused(
            "channel_refused",
            format!("{provider} refused this call ({code}), so it wasn't made."),
        ),
        ProviderCallError::Unsupported => {
            refused("tool_refused", format!("{provider} can't do this through Zupflow yet."))
        }
    }
}

/// The names `messages` are rendered with: the team's identities, the
/// people they mention and the conversations they name, as the
/// connection knows them.
async fn names_for(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run: &RunChannelContext,
    messages: &[ChannelMessage],
) -> Result<MentionNames, &'static str> {
    let provider = run.connection.provider;
    let people: Vec<String> = messages
        .iter()
        .flat_map(|message| message.mentions.iter().cloned())
        .collect();
    let conversations: Vec<String> = messages
        .iter()
        .flat_map(|message| {
            let mut named = conversation_mentions(provider, &message.text);
            if let Some(edited) = &message.edited_text {
                named.extend(conversation_mentions(provider, edited));
            }
            named
        })
        .collect();
    state
        .channels
        .mention_names(
            organization_id,
            workspace_id,
            run.connection.id,
            &people,
            &conversations,
        )
        .await
        .map_err(|_| "store_unavailable")
}

/// What a message says now, as the model reads it.
fn rendered(run: &RunChannelContext, message: &ChannelMessage, names: &MentionNames) -> String {
    plain_text(
        run.connection.provider,
        message.edited_text.as_deref().unwrap_or(&message.text),
        names,
    )
}

/// Messages as a read shows them, rendered with the connection's names.
async fn message_views(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run: &RunChannelContext,
    messages: &[ChannelMessage],
) -> Result<Vec<Value>, &'static str> {
    let names = names_for(state, organization_id, workspace_id, run, messages).await?;
    Ok(messages
        .iter()
        .map(|message| message_view(message, &rendered(run, message, &names)))
        .collect())
}

/// A read of a direct message the run didn't begin in.
const DM_UNREADABLE: &str = "That's a direct message this run didn't start in, so you can't read it.";

/// A message as a tool's result shows it, with `text` as it renders: the
/// author by name, Zupflow's IDs, never the provider's.
fn message_view(message: &ChannelMessage, text: &str) -> Value {
    let from = match (&message.direction, &message.author) {
        (Direction::Outbound, _) => json!({ "you": true }),
        (Direction::Inbound, Some(author)) => json!({
            "person": author.id,
            "name": author.display_name.clone().or_else(|| author.handle.clone()),
            "automated": author.is_automated,
        }),
        (Direction::Inbound, None) => Value::Null,
    };
    let mut view = json!({
        "id": message.id,
        "conversation": message.conversation_id,
        "from": from,
        "sent_at": message.sent_at,
    });
    if let Value::Object(view) = &mut view {
        if message.deleted_at.is_some() {
            view.insert("deleted".to_owned(), json!(true));
        } else {
            view.insert("text".to_owned(), json!(text));
            if message.edited_at.is_some() {
                view.insert("edited".to_owned(), json!(true));
            }
        }
    }
    view
}

fn conversation_view(conversation: &ChannelConversation) -> Value {
    json!({
        "id": conversation.id,
        "name": conversation.name,
        "kind": conversation.kind.as_str(),
        "member": conversation.identity_is_member,
        "shared_with_another_organization": conversation.is_external,
    })
}

fn person_view(person: &KnownPerson) -> Value {
    json!({
        "id": person.id,
        "name": person.display_name,
        "handle": person.handle,
        "automated": person.is_automated,
    })
}

/// Makes one channel call, under its send claim, through the run's own
/// connection: its rules and digest checked once more, then the provider
/// asked with the connection's token.
pub(crate) async fn send(state: &AppState, send: &ChannelSendRequest) -> SendOutcome {
    match make(state, send).await {
        Ok(outcome) => outcome,
        Err(why) => {
            tracing::warn!(event = "channels.tool_not_made", organization_id = %send.organization_id, workspace_id = %send.workspace_id, run_id = %send.run_id, call_id = %send.call_id, why);
            refused(
                "channel_unavailable",
                "The call couldn't be made just now, so it wasn't. Try again.",
            )
        }
    }
}

async fn make(state: &AppState, send: &ChannelSendRequest) -> Result<SendOutcome, &'static str> {
    let (organization_id, workspace_id) = (send.organization_id, send.workspace_id);
    let Some(run) = state
        .channels
        .run_channel(organization_id, workspace_id, send.run_id)
        .await
        .map_err(|_| "store_unavailable")?
    else {
        return Ok(refused(
            CONNECTION_REMOVED,
            "This run's channel connection was removed, so this call wasn't made.",
        ));
    };
    // Only the run's own connection, as it was offered and decided.
    let digest = channel_call_digest(
        run.connection.id,
        &send.provider,
        &send.tool,
        &send.definition_sha256,
        &send.arguments,
        (run.origin.id, run.thread_key.as_str()),
    );
    if run.connection.id != send.connection_id
        || run.connection.provider.as_str() != send.provider
        || digest != send.call_digest
        || tools::definition_sha256(&send.tool) != Some(send.definition_sha256)
    {
        return Ok(refused(
            "tool_changed",
            "This call no longer matches what was decided, so it wasn't made.",
        ));
    }
    if run.connection.status != ConnectionStatus::Connected {
        return Ok(refused(
            CONNECTION_UNUSABLE,
            format!(
                "{} can't reach {} until it's reconnected, so this call wasn't made.",
                run.connection.label,
                run.connection.provider.name()
            ),
        ));
    }
    let target = match resolve(state, organization_id, workspace_id, &run, &send.tool, &send.arguments).await? {
        Ok(target) => target,
        Err(refusal) => {
            let code = if refusal.rule == OUTSIDE_AUDIENCE {
                OUTSIDE_AUDIENCE
            } else {
                "tool_refused"
            };
            return Ok(refused(code, refusal.message));
        }
    };
    let Some(target) = target else {
        return read(state, organization_id, workspace_id, &run, &send.tool, &send.arguments).await;
    };
    let Some(token) = connection_token(state, organization_id, workspace_id, run.connection.id).await? else {
        return Ok(provider_refusal(&run, ProviderCallError::TokenRefused));
    };
    // Every change takes its connection's next send slot first, against
    // every other run sending through it, and waits for it.
    let slot = state
        .channels
        .take_send_slot(
            organization_id,
            workspace_id,
            run.connection.id,
            send.run_id,
            send.call_id,
            state.clock.now(),
        )
        .await
        .map_err(|_| "store_unavailable")?;
    match slot {
        SendSlot::Taken { wait_millis } => {
            if wait_millis > 0 {
                tokio::time::sleep(Duration::from_millis(wait_millis)).await;
            }
        }
        SendSlot::Busy => {
            return Ok(refused(
                RATE_LIMITED,
                format!(
                    "{} is making changes as fast as {} allows, so this call wasn't made. Try again in a few seconds.",
                    run.connection.label,
                    run.connection.provider.name()
                ),
            ));
        }
        SendSlot::HourFull => return Ok(refused(RATE_LIMITED, hour_full(&run))),
    }
    match target {
        Target::Post {
            conversation,
            thread_key,
            text,
        } => {
            let at = PostAt {
                conversation_external_id: conversation.external_id.clone(),
                kind: conversation.kind,
                is_external: conversation.is_external,
                thread_key,
            };
            post(state, send, &run, &token, &at, &text).await
        }
        Target::Direct { person, text } => {
            let provider = state.channel_providers.provider(run.connection.provider);
            let opened = provider
                .open_direct(state.outbound.as_ref(), &token, &person.external_id)
                .await;
            let conversation_external_id = match opened {
                Ok(id) => id,
                Err(error) => return Ok(provider_refusal(&run, error)),
            };
            let at = PostAt {
                conversation_external_id,
                kind: ConversationKind::Dm,
                is_external: false,
                thread_key: String::new(),
            };
            post(state, send, &run, &token, &at, &text).await
        }
        Target::Message {
            conversation,
            message,
            action,
        } => {
            let provider = state.channel_providers.provider(run.connection.provider);
            let http = state.outbound.as_ref();
            let reference = MessageRef {
                conversation_external_id: &conversation.external_id,
                external_id: &message.external_id,
            };
            let now = state.clock.now();
            match action {
                Action::React { emoji, remove } => match provider.react(http, &token, &reference, &emoji, remove).await
                {
                    Ok(()) => Ok(completed(json!({ "done": true }))),
                    Err(error) => Ok(provider_refusal(&run, error)),
                },
                Action::Edit { text } => match provider.edit(http, &token, &reference, &text).await {
                    Ok(()) => {
                        let recorded = state
                            .channels
                            .record_own_edit(organization_id, workspace_id, run.connection.id, message.id, &text, now)
                            .await;
                        if let Err(error) = recorded {
                            tracing::warn!(event = "channels.edit_not_recorded", %organization_id, %workspace_id, message_id = %message.id, %error);
                        }
                        Ok(completed(json!({ "id": message.id, "edited": true })))
                    }
                    Err(error) => Ok(provider_refusal(&run, error)),
                },
                Action::Delete => match provider.delete(http, &token, &reference).await {
                    Ok(()) => {
                        let recorded = state
                            .channels
                            .record_own_delete(organization_id, workspace_id, run.connection.id, message.id, now)
                            .await;
                        if let Err(error) = recorded {
                            tracing::warn!(event = "channels.delete_not_recorded", %organization_id, %workspace_id, message_id = %message.id, %error);
                        }
                        Ok(completed(json!({ "id": message.id, "deleted": true })))
                    }
                    Err(error) => Ok(provider_refusal(&run, error)),
                },
            }
        }
    }
}

/// Where a post goes, by the provider's IDs.
struct PostAt {
    conversation_external_id: String,
    kind: ConversationKind,
    is_external: bool,
    thread_key: String,
}

/// Posts once: a post whose answer was lost is looked for, and recorded
/// when found. Not found, its outcome is unknown: the provider may not show
/// it yet, so it's never made again in the same breath.
async fn post(
    state: &AppState,
    send: &ChannelSendRequest,
    run: &RunChannelContext,
    token: &Zeroizing<String>,
    at: &PostAt,
    text: &str,
) -> Result<SendOutcome, &'static str> {
    let (organization_id, workspace_id) = (send.organization_id, send.workspace_id);
    let send_id = send.call_id.to_string();
    let outgoing = OutgoingPost {
        conversation_external_id: &at.conversation_external_id,
        thread_key: &at.thread_key,
        text,
        send_id: &send_id,
    };
    let provider = state.channel_providers.provider(run.connection.provider);
    let http = state.outbound.as_ref();
    let attempted_at = state.clock.now();
    let posted = match provider.post(http, token, &outgoing).await {
        Ok(external_id) => external_id,
        Err(ProviderCallError::OutcomeUnknown) => {
            match provider.find_sent(http, token, &outgoing, attempted_at).await {
                Ok(Some(external_id)) => external_id,
                // Not found yet is not proof it wasn't sent.
                Ok(None) | Err(_) => {
                    tracing::warn!(event = "channels.post_outcome_unknown", %organization_id, %workspace_id, run_id = %send.run_id, call_id = %send.call_id);
                    return Ok(SendOutcome::Unknown);
                }
            }
        }
        Err(error) => return Ok(provider_refusal(run, error)),
    };
    Ok(record_post(
        state,
        send.run_id,
        send.call_id,
        (organization_id, workspace_id),
        run,
        at,
        text,
        posted,
    )
    .await)
}

/// Records a post as the connection's outbound message, with the run, the
/// call and its hop, and answers with it.
#[allow(clippy::too_many_arguments)]
async fn record_post(
    state: &AppState,
    run_id: RunId,
    call_id: ToolCallId,
    (organization_id, workspace_id): (OrganizationId, WorkspaceId),
    run: &RunChannelContext,
    at: &PostAt,
    text: &str,
    external_id: String,
) -> SendOutcome {
    let now = state.clock.now();
    let recorded = state
        .channels
        .record_message(
            organization_id,
            workspace_id,
            run.connection.id,
            &NewMessage {
                id: ChannelMessageId::generate(),
                conversation: NewConversation {
                    external_id: at.conversation_external_id.clone(),
                    kind: at.kind,
                    name: None,
                    is_external: at.is_external,
                    identity_is_member: true,
                },
                direction: Direction::Outbound,
                author: None,
                external_id,
                thread_key: at.thread_key.clone(),
                text: text.to_owned(),
                mentions: Vec::new(),
                run_id: Some(run_id),
                tool_call_id: Some(call_id),
                agent_hop: run.hop,
                sent_at: now,
            },
            now,
        )
        .await;
    match recorded {
        Ok(recorded) => {
            let message = recorded.message;
            tracing::info!(event = "channels.message_posted", %organization_id, %workspace_id, %run_id, %call_id, connection_id = %run.connection.id, message_id = %message.id);
            completed(json!({
                "id": message.id,
                "conversation": message.conversation_id,
                "posted": true,
            }))
        }
        Err(error) => {
            // Posted: the provider holds it, so the call did what it said.
            tracing::warn!(event = "channels.post_not_recorded", %organization_id, %workspace_id, %run_id, %call_id, %error);
            completed(json!({ "posted": true }))
        }
    }
}

/// A tool that only reads: answered from what the connection kept.
async fn read(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run: &RunChannelContext,
    tool: &str,
    arguments: &Map<String, Value>,
) -> Result<SendOutcome, &'static str> {
    let channels = state.channels.as_ref();
    let connection_id: ChannelConnectionId = run.connection.id;
    let unavailable = |_| "store_unavailable";
    let outcome = match tool {
        "list_conversations" => {
            let kind = string(arguments, "kind").and_then(ConversationKind::parse);
            let filter = ConversationFilter {
                q: string(arguments, "query").map(str::to_owned),
                kind,
            };
            match channels
                .list_conversations(
                    organization_id,
                    workspace_id,
                    connection_id,
                    &filter,
                    string(arguments, "cursor"),
                    MAX_READ,
                )
                .await
            {
                Ok(page) => completed(json!({
                    "conversations": page
                        .items
                        .iter()
                        .filter(|conversation| readable(run, conversation))
                        .map(conversation_view)
                        .collect::<Vec<_>>(),
                    "cursor": page.next_cursor,
                })),
                Err(zupflow_core::channels::store::ChannelError::InvalidCursor) => {
                    tool_error("That cursor isn't one a list gave. Start again without it.")
                }
                Err(_) => return Err("store_unavailable"),
            }
        }
        "get_conversation" => {
            match conversation_named(
                state,
                run,
                organization_id,
                workspace_id,
                string(arguments, "conversation"),
            )
            .await?
            {
                Ok(conversation) if readable(run, &conversation) => completed(conversation_view(&conversation)),
                Ok(_) => tool_error(DM_UNREADABLE),
                Err(refusal) => tool_error(&refusal.message),
            }
        }
        "read_messages" => {
            let conversation = match conversation_named(
                state,
                run,
                organization_id,
                workspace_id,
                string(arguments, "conversation"),
            )
            .await?
            {
                Ok(conversation) if readable(run, &conversation) => conversation,
                Ok(_) => return Ok(tool_error(DM_UNREADABLE)),
                Err(refusal) => return Ok(tool_error(&refusal.message)),
            };
            let limit = arguments
                .get("limit")
                .and_then(Value::as_u64)
                .map_or(MAX_READ, |limit| {
                    u32::try_from(limit.clamp(1, u64::from(MAX_READ))).unwrap_or(MAX_READ)
                });
            match channels
                .list_messages(
                    organization_id,
                    workspace_id,
                    connection_id,
                    conversation.id,
                    &MessageFilter::default(),
                    string(arguments, "cursor"),
                    limit,
                )
                .await
            {
                Ok(page) => completed(json!({
                    "messages": message_views(state, organization_id, workspace_id, run, &page.items).await?,
                    "cursor": page.next_cursor,
                })),
                Err(zupflow_core::channels::store::ChannelError::InvalidCursor) => {
                    tool_error("That cursor isn't one a read gave. Start again without it.")
                }
                Err(_) => return Err("store_unavailable"),
            }
        }
        "read_thread" => {
            let (conversation, message) =
                match message_named(state, run, organization_id, workspace_id, string(arguments, "message")).await? {
                    Ok(found) => found,
                    Err(refusal) => return Ok(tool_error(&refusal.message)),
                };
            if !readable(run, &conversation) {
                return Ok(tool_error(DM_UNREADABLE));
            }
            let thread = if message.thread_key.is_empty() {
                message.external_id.clone()
            } else {
                message.thread_key.clone()
            };
            let page = channels
                .list_messages(
                    organization_id,
                    workspace_id,
                    connection_id,
                    conversation.id,
                    &MessageFilter { thread: Some(thread) },
                    None,
                    MAX_READ,
                )
                .await
                .map_err(unavailable)?;
            let mut messages = message_views(state, organization_id, workspace_id, run, &page.items).await?;
            messages.reverse();
            completed(json!({ "conversation": conversation.id, "messages": messages }))
        }
        "search_messages" => {
            let Some(query) = string(arguments, "query") else {
                return Ok(tool_error("Give the text to search for."));
            };
            let conversation = match string(arguments, "conversation") {
                None => None,
                Some(named) => {
                    match conversation_named(state, run, organization_id, workspace_id, Some(named)).await? {
                        Ok(conversation) if readable(run, &conversation) => Some(conversation.id),
                        Ok(_) => return Ok(tool_error(DM_UNREADABLE)),
                        Err(refusal) => return Ok(tool_error(&refusal.message)),
                    }
                }
            };
            // The store finds what may match; what's shown matches as the
            // model reads it, names and all.
            let candidates = channels
                .search_messages(
                    organization_id,
                    workspace_id,
                    connection_id,
                    conversation,
                    run.origin.id,
                    query,
                    SEARCH_CANDIDATES,
                )
                .await
                .map_err(unavailable)?;
            let names = names_for(state, organization_id, workspace_id, run, &candidates).await?;
            let wanted = query.to_lowercase();
            let found: Vec<Value> = candidates
                .iter()
                .filter_map(|message| {
                    let text = rendered(run, message, &names);
                    text.to_lowercase()
                        .contains(&wanted)
                        .then(|| message_view(message, &text))
                })
                .take(usize::try_from(MAX_FOUND).unwrap_or(usize::MAX))
                .collect();
            completed(json!({ "messages": found }))
        }
        "find_people" => {
            let Some(query) = string(arguments, "query") else {
                return Ok(tool_error("Give part of their name or handle."));
            };
            let found = channels
                .find_people(organization_id, workspace_id, connection_id, query, MAX_FOUND)
                .await
                .map_err(unavailable)?;
            completed(json!({ "people": found.iter().map(person_view).collect::<Vec<_>>() }))
        }
        "get_person" => {
            let person = match string(arguments, "person").and_then(|named| ChannelPersonId::parse(named).ok()) {
                Some(id) => channels
                    .find_person(organization_id, workspace_id, connection_id, id)
                    .await
                    .map_err(unavailable)?,
                None => None,
            };
            match person {
                Some(person) => completed(person_view(&person)),
                None => tool_error("That person isn't one this connection knows. Use an ID find_people gives."),
            }
        }
        _ => refused("tool_refused", format!("{tool} isn't a tool this run can use.")),
    };
    Ok(outcome)
}

// --- A send whose sender stopped -------------------------------------------------

/// Settles a channel post whose sender stopped before recording what came
/// of it, by finding the post: recorded already, or held by the provider
/// (then recorded now), it completed; the provider looked and it wasn't
/// sent, it failed, and the model may post again. `None` when it can't be
/// told, or for a call that isn't a post: the call is settled unknown.
pub(crate) async fn settle_lapsed(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    run_id: RunId,
    call: &CallRecord,
    attempted_at: Timestamp,
) -> Option<SendOutcome> {
    let target = call.target.as_ref()?;
    target.channel()?;
    if !tools::posts(&target.tool) {
        return None;
    }
    let recorded = state
        .channels
        .sent_by_call(organization_id, workspace_id, run_id, call.id)
        .await
        .ok()?;
    if let Some(message) = recorded {
        return Some(completed(json!({
            "id": message.id,
            "conversation": message.conversation_id,
            "posted": true,
        })));
    }
    let run = state
        .channels
        .run_channel(organization_id, workspace_id, run_id)
        .await
        .ok()??;
    let at = match &target.tool[..] {
        "reply" => PostAt {
            conversation_external_id: run.origin.external_id.clone(),
            kind: run.origin.kind,
            is_external: run.origin.is_external,
            thread_key: run.thread_key.clone(),
        },
        // Where a message to someone or another conversation went is read
        // again as it was decided.
        _ => match resolve(
            state,
            organization_id,
            workspace_id,
            &run,
            &target.tool,
            &call.arguments,
        )
        .await
        .ok()?
        .ok()??
        {
            Target::Post {
                conversation,
                thread_key,
                ..
            } => PostAt {
                conversation_external_id: conversation.external_id,
                kind: conversation.kind,
                is_external: conversation.is_external,
                thread_key,
            },
            // A direct message's conversation is the provider's to name.
            _ => return None,
        },
    };
    let text = string(&call.arguments, "text")?.to_owned();
    let token = connection_token(state, organization_id, workspace_id, run.connection.id)
        .await
        .ok()??;
    let send_id = call.id.to_string();
    let outgoing = OutgoingPost {
        conversation_external_id: &at.conversation_external_id,
        thread_key: &at.thread_key,
        text: &text,
        send_id: &send_id,
    };
    match state
        .channel_providers
        .provider(run.connection.provider)
        .find_sent(state.outbound.as_ref(), &token, &outgoing, attempted_at)
        .await
    {
        Ok(Some(external_id)) => Some(
            record_post(
                state,
                run_id,
                call.id,
                (organization_id, workspace_id),
                &run,
                &at,
                &text,
                external_id,
            )
            .await,
        ),
        Ok(None) => Some(refused(
            "channel_not_sent",
            "The message wasn't posted: the attempt stopped before it went out. Post it again if it's still needed.",
        )),
        Err(_) => None,
    }
}

// --- Typing --------------------------------------------------------------------

/// A thread that shows its agent working: the connection's own provider,
/// and what it's told with.
#[derive(Clone)]
struct Shown {
    provider: Arc<dyn ChannelProvider>,
    http: Arc<dyn OutboundHttp>,
    token: Zeroizing<String>,
    conversation_external_id: String,
    thread_key: String,
}

impl Shown {
    async fn show(&self, working: bool) -> bool {
        self.provider
            .show_working(
                self.http.as_ref(),
                &self.token,
                &self.conversation_external_id,
                &self.thread_key,
                working,
            )
            .await
    }
}

/// The thread showing that a channel run's agent is working, kept alive in
/// the background until [`Working::stop`]. Best effort throughout.
pub(crate) struct Working {
    task: tokio::task::JoinHandle<()>,
    clear: Option<Shown>,
}

impl Working {
    /// Shows it now, then every 10 seconds; `None` when the thread can't be
    /// told (the connection isn't connected, or has no token).
    pub(crate) async fn start(
        state: &AppState,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        origin: &ChannelOrigin,
    ) -> Option<Self> {
        let connection = state
            .channels
            .find_connection(organization_id, workspace_id, origin.connection_id)
            .await
            .ok()??;
        if connection.status != ConnectionStatus::Connected {
            return None;
        }
        let conversation = state
            .channels
            .find_conversation(
                organization_id,
                workspace_id,
                origin.connection_id,
                origin.conversation_id,
            )
            .await
            .ok()??;
        let token = connection_token(state, organization_id, workspace_id, origin.connection_id)
            .await
            .ok()??;
        let shown = Shown {
            // The connection's own provider shows it.
            provider: state.channel_providers.provider(connection.provider),
            http: state.outbound.clone(),
            token,
            conversation_external_id: conversation.external_id,
            thread_key: origin.thread_key.clone(),
        };
        if !shown.show(true).await {
            return None;
        }
        let task = {
            let shown = shown.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(WORKING_EVERY).await;
                    shown.show(true).await;
                }
            })
        };
        Some(Self {
            task,
            clear: Some(shown),
        })
    }

    /// Stops showing it.
    pub(crate) async fn stop(mut self) {
        self.task.abort();
        if let Some(shown) = self.clear.take() {
            shown.show(false).await;
        }
    }
}

impl Drop for Working {
    fn drop(&mut self) {
        self.task.abort();
    }
}
