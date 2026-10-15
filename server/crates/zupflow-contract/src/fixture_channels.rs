//! The reference fixture's channels: in each workspace named, a linked team,
//! one agent's app made and installed with its connection, that agent's
//! channel on it (tools `read_thread`, `reply` and `send_message`, approval
//! for `send_message`), and two conversations: `support`, a channel with a
//! three-message thread (one message the agent's own), and a DM with one
//! message. Everything is written through the store, as the API and the
//! provisioning job write it, with tokens and secrets sealed as they seal them.

use serde_json::json;
use sha2::{Digest, Sha256};
use zupflow_basic::local_keys::LocalKeyService;
use zupflow_core::agents::Agent;
use zupflow_core::audit::{AuditAction, AuditObject, AuditPlace, AuditTarget};
use zupflow_core::channels::provision::SLACK_SCOPES;
use zupflow_core::channels::store::{
    ChannelEditor, ChannelStore, MadeApp, NewApp, NewConversation, NewInstall, NewManager, NewMessage, NewPerson,
    SealedVersion,
};
use zupflow_core::channels::{
    AgentChannel, AgentChannelSettings, ChannelApp, ChannelAppManager, ChannelProviderKind, ConnectionTeam,
    ConversationKind, Direction, FIRST_VERSION, Identity, InboundRef, ManagerMethod, TOKEN_LIFETIME_MS,
};
use zupflow_core::connect::Reference;
use zupflow_core::environment::Environment;
use zupflow_core::ids::{
    ChannelAppId, ChannelAppManagerId, ChannelConnectionId, ChannelMessageId, MemberId, OrganizationId, UserId,
    WorkspaceId,
};
use zupflow_core::jobs::JobOptions;
use zupflow_core::keys::{EncryptionContext, Sealed, seal};
use zupflow_core::runs::QueuedJob;
use zupflow_core::scope::Name;
use zupflow_core::time::Timestamp;

use crate::by_fixture;

/// A workspace's channels in the fixture.
pub struct Seed {
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub environment: Environment,
    /// The member who links the team: an owner, as the store checks.
    pub editor: MemberId,
    pub linked_by: UserId,
    pub agent: Agent,
    pub team: (&'static str, &'static str),
    /// The people who write: provider ID, display name, handle, email.
    pub people: &'static [(&'static str, &'static str, &'static str, &'static str)],
}

const MINUTE: i64 = 60_000;

/// `length` characters of Slack's ID alphabet, derived from `seed`.
fn code(seed: &str, length: usize) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    Sha256::digest(seed.as_bytes())
        .iter()
        .take(length)
        .map(|byte| char::from(ALPHABET[usize::from(byte % 32)]))
        .collect()
}

fn random() -> Result<String, String> {
    Reference::generate()
        .map(|reference| reference.expose().to_owned())
        .map_err(|error| format!("random: {error:?}"))
}

/// A provider's timestamp for an instant: seconds with six decimals.
fn ts(at: Timestamp) -> String {
    let millis = at.millis();
    format!("{}.{:06}", millis.div_euclid(1000), millis.rem_euclid(1000) * 1000)
}

async fn sealed(
    keys: &LocalKeyService,
    context: &EncryptionContext,
    value: &serde_json::Value,
) -> Result<Sealed, String> {
    let plaintext = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    seal(keys, context, &plaintext)
        .await
        .map_err(|error| format!("seal: {error:?}"))
}

pub async fn seed(channels: &dyn ChannelStore, keys: &LocalKeyService, seed: &Seed) -> Result<(), String> {
    let fail =
        |what: &'static str| move |error: zupflow_core::channels::store::ChannelError| format!("{what}: {error:?}");
    let (organization_id, workspace_id) = (seed.organization_id, seed.workspace_id);
    let editor = ChannelEditor::Member(seed.editor);
    let place = AuditPlace::Workspace(workspace_id);
    let now = Timestamp::now();

    let manager_id = ChannelAppManagerId::generate();
    let configuration_token = format!("xoxe.xoxp-1-fixture-{}", code(&workspace_id.to_string(), 8));
    let credential = sealed(
        keys,
        &EncryptionContext::channel_app_manager_credential(
            organization_id,
            workspace_id,
            seed.environment,
            manager_id,
            FIRST_VERSION,
        ),
        &json!({ "configuration_token": configuration_token, "refresh_token": format!("xoxe-1-{}", random()?) }),
    )
    .await?;
    let manager = channels
        .create_manager(
            organization_id,
            workspace_id,
            manager_id,
            editor,
            &NewManager {
                provider: ChannelProviderKind::Slack,
                method: ManagerMethod::ConfigurationToken,
                credential,
                token_last4: configuration_token[configuration_token.len() - 4..].to_owned(),
                token_expires_at: Some(Timestamp::from_millis(now.millis() + TOKEN_LIFETIME_MS)),
                created_by: Some(seed.linked_by),
            },
            now,
            &|manager: &ChannelAppManager| {
                by_fixture(
                    AuditAction::ChannelAppManagerCreated,
                    AuditTarget::new(AuditObject::ChannelAppManager(manager.id), "Slack"),
                    place,
                )
            },
        )
        .await
        .map_err(fail("channel manager"))?;

    // Made as the job makes it; the job queued with it is due only once
    // that's recorded (a worker is running), so it finds it made and does nothing.
    let app_id = ChannelAppId::generate();
    let kind = Name::new(zupflow_server::channels::PROVISION_CHANNEL_APP).map_err(|error| format!("{error:?}"))?;
    let inbound_ref = InboundRef::generate().map_err(|error| format!("inbound reference: {error}"))?;
    channels
        .create_app(
            organization_id,
            workspace_id,
            app_id,
            editor,
            &NewApp {
                agent_id: seed.agent.id,
                manager_id: manager.id,
                inbound_ref,
            },
            &QueuedJob {
                kind,
                payload: json!({ "app_id": app_id.to_string() }).to_string().into_bytes(),
                options: JobOptions {
                    delay: std::time::Duration::from_secs(60),
                    ..JobOptions::default()
                },
            },
            now,
            &|app: &ChannelApp| {
                by_fixture(
                    AuditAction::ChannelAppCreated,
                    AuditTarget::new(AuditObject::ChannelApp(app.id), &seed.agent.name),
                    place,
                )
            },
        )
        .await
        .map_err(fail("channel app"))?;
    let secrets = sealed(
        keys,
        &EncryptionContext::channel_app_secret(organization_id, workspace_id, seed.environment, app_id, FIRST_VERSION),
        &json!({ "client_secret": random()?, "signing_secret": random()? }),
    )
    .await?;
    let app_code = code(&app_id.to_string(), 10);
    channels
        .record_app_made(
            organization_id,
            workspace_id,
            app_id,
            &MadeApp {
                external_app_id: format!("A0{}", &app_code[..9]),
                client_id: format!("1000.{app_code}"),
                secrets,
            },
            now,
        )
        .await
        .map_err(fail("app made"))?;

    let connection_id = ChannelConnectionId::generate();
    let token = sealed(
        keys,
        &EncryptionContext::channel_connection_credential(
            organization_id,
            workspace_id,
            seed.environment,
            connection_id,
            FIRST_VERSION,
        ),
        &json!({ "token": format!("xoxb-fixture-{}", random()?) }),
    )
    .await?;
    let identity = format!("U0{}", code(&connection_id.to_string(), 8));
    let handle: String = seed
        .agent
        .name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let (connection, _) = channels
        .install_app(
            organization_id,
            workspace_id,
            app_id,
            &NewInstall {
                connection_id,
                team: ConnectionTeam {
                    id: seed.team.0.to_owned(),
                    name: Some(seed.team.1.to_owned()),
                    enterprise_id: None,
                },
                identity: Identity {
                    external_id: identity.clone(),
                    handle: handle.trim_matches('-').to_owned(),
                },
                granted_scopes: SLACK_SCOPES.iter().map(|scope| (*scope).to_owned()).collect(),
                label: seed.agent.name.chars().take(40).collect(),
                credential: SealedVersion {
                    version: FIRST_VERSION,
                    sealed: token,
                },
            },
            now,
        )
        .await
        .map_err(fail("install"))?;

    // A thread in `support` and a DM, as the inbound job records them.
    let person = |index: usize| {
        let (external_id, display_name, handle, email) = seed.people[index.min(seed.people.len() - 1)];
        NewPerson {
            external_id: external_id.to_owned(),
            handle: Some(handle.to_owned()),
            display_name: Some(display_name.to_owned()),
            email: Some(email.to_owned()),
            is_automated: false,
        }
    };
    let support = NewConversation {
        external_id: format!("C0{}", code(&format!("{workspace_id}:support"), 8)),
        kind: ConversationKind::Channel,
        name: Some("support".to_owned()),
        is_external: false,
        identity_is_member: true,
    };
    let dm = NewConversation {
        external_id: format!("D0{}", code(&format!("{workspace_id}:dm"), 8)),
        kind: ConversationKind::Dm,
        name: None,
        is_external: false,
        identity_is_member: true,
    };
    let root = ts(Timestamp::from_millis(now.millis() - 50 * MINUTE));
    let mention = format!("<@{identity}>");
    for (conversation, author, minutes_ago, text, thread) in [
        (
            &support,
            Some(person(0)),
            50,
            format!("{mention} can you check order 4521? It hasn't arrived."),
            root.clone(),
        ),
        (
            &support,
            None,
            49,
            "It shipped Monday and is out for delivery today.".to_owned(),
            root.clone(),
        ),
        (
            &support,
            Some(person(1)),
            40,
            format!("Thanks {mention}, that's what I needed."),
            root.clone(),
        ),
        (
            &dm,
            Some(person(0)),
            180,
            "What's on the board for this week?".to_owned(),
            String::new(),
        ),
    ] {
        let sent_at = Timestamp::from_millis(now.millis() - minutes_ago * MINUTE);
        let inbound = author.is_some();
        channels
            .record_message(
                organization_id,
                workspace_id,
                connection.id,
                &NewMessage {
                    id: ChannelMessageId::generate(),
                    conversation: conversation.clone(),
                    direction: if inbound {
                        Direction::Inbound
                    } else {
                        Direction::Outbound
                    },
                    author,
                    external_id: ts(sent_at),
                    thread_key: thread,
                    text,
                    mentions: if inbound { vec![identity.clone()] } else { Vec::new() },
                    run_id: None,
                    tool_call_id: None,
                    agent_hop: 0,
                    sent_at,
                },
                now,
            )
            .await
            .map_err(fail("message"))?;
    }

    let names = |tools: &[&str]| tools.iter().map(|tool| (*tool).to_owned()).collect::<Vec<_>>();
    channels
        .put_agent_channel(
            organization_id,
            workspace_id,
            seed.agent.id,
            connection.id,
            editor,
            &AgentChannelSettings {
                tools: names(&["read_thread", "reply", "send_message"]),
                approval_required: names(&["send_message"]),
                ..AgentChannelSettings::default()
            },
            now,
            &|channel: &AgentChannel| {
                by_fixture(
                    AuditAction::AgentChannelUpdated,
                    AuditTarget::new(
                        AuditObject::AgentChannel(channel.id),
                        format!("{} · {}", seed.agent.name, connection.label),
                    ),
                    place,
                )
            },
        )
        .await
        .map_err(fail("agent channel"))?;
    Ok(())
}
