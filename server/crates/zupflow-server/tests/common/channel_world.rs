//! A team's chat reaching a workspace's agents, for the channel run tests:
//! [`run_setup`]'s world, with each agent's app made by its job, installed
//! in a fake team ([`FakeChat`]) as the consent callback would install it,
//! and every message delivered to the apps' inbound endpoint signed with
//! each app's own secret, through the HTTP route, on the kit's clock.
//!
//! Include it beside `common`, `faults` and `run_setup` with
//! `#[path = "common/channel_world.rs"] mod channel_world;`.

#![allow(dead_code, clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

use zupflow_channels::fake::{ChatMessage, FakeChannelProvider, FakeChat, Poster, SLACK_HEADERS, sign};
use zupflow_core::channels::store::{NewConversation, NewInstall, NewMessage, SealedVersion};
use zupflow_core::channels::{ConnectionTeam, ConversationKind, Direction, FIRST_VERSION, Identity};
use zupflow_core::ids::{ChannelAppId, ChannelConnectionId, ChannelMessageId, OrganizationId, RunId, WorkspaceId};
use zupflow_core::keys::{self, EncryptionContext};
use zupflow_core::runs::CallState;
use zupflow_server::RequestGuard;

use crate::common::call;
use crate::faults::Worker;
use crate::run_setup::{Setup, id};

pub const TEAM: &str = "T0NORTH";
pub const SUPPORT: &str = "C0SUPPORT";
pub const ANA: &str = "U0ANA";
pub const BO: &str = "U0BO";

const CONFIGURATION_TOKEN: &str = "xoxe.xoxp-1-server-test-7q2k";

/// One agent's app, installed in a team: what the provider holds for it, and
/// where it lives.
#[derive(Clone)]
pub struct Installed {
    pub user: String,
    pub org: String,
    pub ws: String,
    pub agent: String,
    pub app: String,
    pub connection: String,
    /// The provider's ID of the identity the agent answers as.
    pub identity: String,
    /// The bot ID its posts carry, as the install reported it.
    pub bot_id: Option<String>,
    pub inbound_ref: String,
    pub signing_secret: Vec<u8>,
}

impl Installed {
    pub fn scope(&self) -> (OrganizationId, WorkspaceId) {
        (
            OrganizationId::parse(&self.org).unwrap(),
            WorkspaceId::parse(&self.ws).unwrap(),
        )
    }

    /// `<@identity>`, as the provider writes a mention.
    pub fn mention(&self) -> String {
        format!("<@{}>", self.identity)
    }

    /// The agent posting through its app, as Slack delivers its own post:
    /// its user and its bot ID.
    pub fn poster(&self) -> Poster {
        Poster::app(&self.identity, &bot_id_of(&self.identity))
    }
}

/// The bot ID the fake team gives an identity: `U0HELPER`'s is `B0HELPER`.
pub fn bot_id_of(identity: &str) -> String {
    format!("B{}", identity.trim_start_matches('U'))
}

/// An app made but not installed yet: where its deliveries go, and what
/// they're signed with.
pub struct Made {
    pub app: String,
    pub inbound_ref: String,
    pub signing_secret: Vec<u8>,
}

pub struct World {
    pub setup: Setup,
    /// The team's chat: what people post, and what the agents' apps do in
    /// it with their tokens.
    pub chat: Arc<FakeChat>,
    /// The API on the kit's clock, as the provider reaches it.
    pub api: axum::Router,
    /// The setup's agent, installed in [`TEAM`].
    pub main: Installed,
}

pub async fn world(mut setup: Setup) -> World {
    let chat = Arc::new(FakeChat::new(TEAM));
    setup
        .kit
        .use_channel_provider(Arc::new(FakeChannelProvider::in_chat(chat.clone())));
    let api = zupflow_server::app(setup.kit.state(Worker::Api), RequestGuard::default());
    let mut world = World {
        chat,
        api,
        main: placeholder(),
        setup,
    };
    let (org, ws, agent) = (
        world.setup.org.clone(),
        world.setup.ws.clone(),
        world.setup.agent.clone(),
    );
    world.main = world
        .install("alice", &org, &ws, &agent, TEAM, "U0HELPER", "helper")
        .await;
    world
}

fn placeholder() -> Installed {
    Installed {
        user: String::new(),
        org: String::new(),
        ws: String::new(),
        agent: String::new(),
        app: String::new(),
        connection: String::new(),
        identity: String::new(),
        bot_id: None,
        inbound_ref: String::new(),
        signing_secret: Vec::new(),
    }
}

impl World {
    pub fn now(&self) -> zupflow_core::time::Timestamp {
        use zupflow_core::time::Clock;
        self.setup.kit.clock.now()
    }

    /// Makes `agent`'s app with its job and installs it in `team` as
    /// `identity`, its bot ID reported: what the consent callback does once
    /// it's served.
    #[allow(clippy::too_many_arguments)]
    pub async fn install(
        &self,
        user: &str,
        org: &str,
        ws: &str,
        agent: &str,
        team: &str,
        identity: &str,
        handle: &str,
    ) -> Installed {
        let bot_id = bot_id_of(identity);
        self.install_reporting(user, org, ws, agent, team, identity, handle, Some(&bot_id))
            .await
    }

    /// [`Self::install`], with the bot ID the install reports, if any.
    #[allow(clippy::too_many_arguments)]
    pub async fn install_reporting(
        &self,
        user: &str,
        org: &str,
        ws: &str,
        agent: &str,
        team: &str,
        identity: &str,
        handle: &str,
        bot_id: Option<&str>,
    ) -> Installed {
        let made = self.make_app(user, org, ws, agent).await;
        let app = made.app.clone();
        let (organization_id, workspace_id) = (OrganizationId::parse(org).unwrap(), WorkspaceId::parse(ws).unwrap());
        let app_id = ChannelAppId::parse(&app).unwrap();
        let kit = &self.setup.kit;
        let state = kit.state(Worker::A);
        let environment = state
            .workspaces
            .find_workspace(organization_id, workspace_id)
            .await
            .unwrap()
            .unwrap()
            .environment;
        let connection_id = ChannelConnectionId::generate();
        // Each app's own token, which the chat knows it by.
        let token = format!("xoxb-{identity}");
        self.chat.install(&token, Poster::app(identity, &bot_id_of(identity)));
        let sealed = keys::seal(
            state.keys.as_ref(),
            &EncryptionContext::channel_connection_credential(
                organization_id,
                workspace_id,
                environment,
                connection_id,
                FIRST_VERSION,
            ),
            json!({ "token": token }).to_string().as_bytes(),
        )
        .await
        .unwrap();
        let (connection, _) = state
            .channels
            .install_app(
                organization_id,
                workspace_id,
                app_id,
                &NewInstall {
                    connection_id,
                    team: ConnectionTeam {
                        id: team.to_owned(),
                        name: Some("Northwind".to_owned()),
                        enterprise_id: None,
                    },
                    identity: Identity {
                        external_id: identity.to_owned(),
                        handle: handle.to_owned(),
                        bot_id: bot_id.map(str::to_owned),
                    },
                    granted_scopes: Vec::new(),
                    label: handle.to_owned(),
                    credential: SealedVersion {
                        version: FIRST_VERSION,
                        sealed,
                    },
                },
                self.now(),
            )
            .await
            .unwrap();
        Installed {
            user: user.to_owned(),
            org: org.to_owned(),
            ws: ws.to_owned(),
            agent: agent.to_owned(),
            app,
            connection: connection.id.to_string(),
            identity: identity.to_owned(),
            bot_id: bot_id.map(str::to_owned),
            inbound_ref: made.inbound_ref,
            signing_secret: made.signing_secret,
        }
    }

    /// Makes `agent`'s app with its job, and reads what the provider holds
    /// for it: the app is `awaiting_install`.
    pub async fn make_app(&self, user: &str, org: &str, ws: &str, agent: &str) -> Made {
        let kit = &self.setup.kit;
        let managers = format!("/v1/workspaces/{ws}/channel-app-managers");
        let (status, listed) = kit.send(call(user, Method::GET, managers.clone()).naming(org)).await;
        assert_eq!(status, StatusCode::OK, "{listed}");
        let manager = match listed["data"].as_array().unwrap().first() {
            Some(manager) => id(manager).to_owned(),
            None => {
                let (status, linked) = kit
                    .send(call(user, Method::POST, managers).naming(org).body(json!({
                        "provider": "slack",
                        "method": "configuration_token",
                        "configuration_token": CONFIGURATION_TOKEN,
                        "refresh_token": "xoxe-1-server-refresh-9f3d",
                    })))
                    .await;
                assert_eq!(status, StatusCode::CREATED, "{linked}");
                id(&linked["data"]).to_owned()
            }
        };
        let (status, accepted) = kit
            .send(
                call(user, Method::POST, format!("/v1/workspaces/{ws}/channel-apps"))
                    .naming(org)
                    .body(json!({ "agent_id": agent, "manager_id": manager })),
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
        let app = id(&accepted["data"]).to_owned();
        // Made by its job, once the job is due.
        let mut made = false;
        for _ in 0..100 {
            zupflow_server::channels::work_once(&kit.state(Worker::A)).await;
            let (status, read) = kit
                .send(call(user, Method::GET, format!("/v1/workspaces/{ws}/channel-apps/{app}")).naming(org))
                .await;
            assert_eq!(status, StatusCode::OK, "{read}");
            if read["data"]["status"] != "creating" {
                assert_eq!(read["data"]["status"], "awaiting_install", "{read}");
                made = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(made, "{app} is still being made");
        let (organization_id, workspace_id) = (OrganizationId::parse(org).unwrap(), WorkspaceId::parse(ws).unwrap());
        let app_id = ChannelAppId::parse(&app).unwrap();
        let state = kit.state(Worker::A);
        let environment = state
            .workspaces
            .find_workspace(organization_id, workspace_id)
            .await
            .unwrap()
            .unwrap()
            .environment;
        let record = state
            .channels
            .app_secrets(organization_id, workspace_id, app_id)
            .await
            .unwrap()
            .unwrap();
        let secrets = record.secrets.unwrap();
        let opened = keys::open(
            state.keys.as_ref(),
            &EncryptionContext::channel_app_secret(organization_id, workspace_id, environment, app_id, secrets.version),
            &secrets.sealed,
        )
        .await
        .unwrap();
        let opened: Value = serde_json::from_slice(opened.expose()).unwrap();
        Made {
            app,
            inbound_ref: record.inbound_ref.expose().to_owned(),
            signing_secret: opened["signing_secret"].as_str().unwrap().as_bytes().to_vec(),
        }
    }

    /// Delivers `body` to `to`'s inbound endpoint, signed with `secret`
    /// (its own when `None`) at the kit's time: the status and the answer.
    pub async fn deliver_signed(&self, to: &Installed, body: &Value, secret: Option<&[u8]>) -> (StatusCode, Value) {
        self.deliver_to(&to.inbound_ref, body, secret.unwrap_or(&to.signing_secret))
            .await
    }

    /// Delivers `body` to the inbound endpoint of the app `inbound_ref`
    /// names, signed with `secret` at the kit's time.
    pub async fn deliver_to(&self, inbound_ref: &str, body: &Value, secret: &[u8]) -> (StatusCode, Value) {
        let signed = sign(body, secret, self.now());
        self.deliver_raw(inbound_ref, signed.body, &signed.timestamp, &signed.signature)
            .await
    }

    /// Sends `body` as signed to the inbound endpoint of `inbound_ref`.
    pub async fn deliver_raw(
        &self,
        inbound_ref: &str,
        body: Vec<u8>,
        timestamp: &str,
        signature: &str,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("POST")
            .uri(format!("/channels/inbound/{inbound_ref}"))
            .header("Content-Type", "application/json")
            .header(SLACK_HEADERS.timestamp, timestamp)
            .header(SLACK_HEADERS.signature, signature)
            .body(Body::from(body))
            .unwrap();
        let response = self.api.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    pub async fn deliver(&self, to: &Installed, body: &Value) -> (StatusCode, Value) {
        let (status, answer) = self.deliver_signed(to, body, None).await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        (status, answer)
    }

    /// Someone posts in the chat; every app in `to` gets the delivery.
    pub async fn post(
        &self,
        to: &[&Installed],
        channel: &str,
        user: &str,
        text: &str,
        thread: Option<&str>,
    ) -> ChatMessage {
        let kind = if channel.starts_with('D') { "im" } else { "channel" };
        let message = self.chat.post(channel, kind, user, text, thread, self.now());
        for app in to {
            self.deliver(app, &self.chat.delivery(&message)).await;
        }
        message
    }

    /// `poster` posts in the chat; every app in `to` gets the delivery.
    pub async fn post_as(
        &self,
        to: &[&Installed],
        channel: &str,
        poster: &Poster,
        text: &str,
        thread: Option<&str>,
    ) -> ChatMessage {
        let kind = if channel.starts_with('D') { "im" } else { "channel" };
        let message = self.chat.post_as(channel, kind, poster, text, thread, self.now());
        for app in to {
            self.deliver(app, &self.chat.delivery(&message)).await;
        }
        message
    }

    /// Someone posts in `SUPPORT`, to the setup's agent.
    pub async fn say(&self, user: &str, text: &str, thread: Option<&str>) -> ChatMessage {
        self.post(&[&self.main], SUPPORT, user, text, thread).await
    }

    /// The inbound jobs due, handled.
    pub async fn handle(&self) {
        let state = self.setup.kit.state(Worker::A);
        while zupflow_server::channel_inbound::work_once(&state).await {}
    }

    /// The replies due, posted.
    pub async fn reply(&self) {
        let state = self.setup.kit.state(Worker::A);
        while zupflow_server::channel_reply::work_once(&state).await {}
    }

    /// The inbound jobs, then every run, worked as far as they go.
    pub async fn work(&self) {
        self.handle().await;
        self.setup.kit.drain(Worker::A).await;
    }

    /// `installed`'s agent's channel runs, oldest first.
    pub async fn runs(&self, installed: &Installed) -> Vec<Value> {
        let (status, body) = self
            .setup
            .kit
            .send(
                call(
                    &installed.user,
                    Method::GET,
                    format!(
                        "/v1/workspaces/{}/runs?agent_id={}&limit=100",
                        installed.ws, installed.agent
                    ),
                )
                .naming(&installed.org),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let mut runs: Vec<Value> = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|run| run["trigger"] == "channel")
            .cloned()
            .collect();
        runs.sort_by(|a, b| a["created_at"].as_str().cmp(&b["created_at"].as_str()));
        runs
    }

    pub fn run_id(run: &Value) -> RunId {
        RunId::parse(id(run)).unwrap()
    }

    /// Changes `installed`'s agent channel settings: its current ones with
    /// `changes` over them.
    pub async fn configure(&self, installed: &Installed, changes: Value) {
        let kit = &self.setup.kit;
        let channels = format!("/v1/workspaces/{}/agents/{}/channels", installed.ws, installed.agent);
        let (status, listed) = kit
            .send(call(&installed.user, Method::GET, channels.clone()).naming(&installed.org))
            .await;
        assert_eq!(status, StatusCode::OK, "{listed}");
        let mut settings = listed["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|channel| channel["connection_id"] == installed.connection.as_str())
            .unwrap()
            .clone();
        for (field, value) in changes.as_object().unwrap() {
            settings[field] = value.clone();
        }
        let (status, saved) = kit
            .send(
                call(
                    &installed.user,
                    Method::PUT,
                    format!("{channels}/{}", installed.connection),
                )
                .naming(&installed.org)
                .body(settings),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{saved}");
    }

    /// `installed`'s connection as the API reads it.
    pub async fn connection(&self, installed: &Installed) -> Value {
        let (status, body) = self
            .setup
            .kit
            .send(
                call(
                    &installed.user,
                    Method::GET,
                    format!(
                        "/v1/workspaces/{}/channel-connections/{}",
                        installed.ws, installed.connection
                    ),
                )
                .naming(&installed.org),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["data"].clone()
    }

    /// The name `installed`'s agent calls its channel tool `tool` by.
    pub async fn tool(&self, installed: &Installed, tool: &str) -> String {
        let connection = self.connection(installed).await;
        format!("slack_{}_{tool}", connection["short_id"].as_str().unwrap())
    }

    /// The line that has the model call `installed`'s channel tool `tool`.
    pub async fn calling(&self, installed: &Installed, tool: &str, arguments: Value) -> String {
        format!("call {} {arguments}", self.tool(installed, tool).await)
    }

    /// Grants `installed`'s agent these channel tools, and no others: each
    /// the organization's levels approve first is granted needing approval.
    pub async fn grant(&self, installed: &Installed, tools: &[&str]) {
        let approved: Vec<&str> = tools
            .iter()
            .copied()
            .filter(|tool| {
                zupflow_core::channels::channel_tool(tool)
                    .is_some_and(|(_, level)| level == zupflow_core::policy::ToolLevel::Approve)
            })
            .collect();
        self.configure(installed, json!({ "tools": tools, "approval_required": approved }))
            .await;
    }

    /// The ID `installed`'s connection gives the conversation `external_id`.
    pub async fn conversation_id(&self, installed: &Installed, external_id: &str) -> String {
        self.conversations(installed)
            .await
            .into_iter()
            .find(|conversation| conversation["external_id"] == external_id)
            .map(|conversation| id(&conversation).to_owned())
            .unwrap()
    }

    /// The ID `installed`'s connection gives the message `message`.
    pub async fn message_id(&self, installed: &Installed, message: &ChatMessage) -> String {
        self.messages_in(installed, &message.channel)
            .await
            .into_iter()
            .find(|held| held["external_id"] == message.ts.as_str())
            .map(|held| id(&held).to_owned())
            .unwrap()
    }

    /// The conversation's messages in `installed`'s workspace, newest first.
    pub async fn conversations(&self, installed: &Installed) -> Vec<Value> {
        let (status, body) = self
            .setup
            .kit
            .send(
                call(
                    &installed.user,
                    Method::GET,
                    format!(
                        "/v1/workspaces/{}/channel-connections/{}/conversations?limit=100",
                        installed.ws, installed.connection
                    ),
                )
                .naming(&installed.org),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["data"].as_array().unwrap().clone()
    }

    /// The messages of `installed`'s conversation `external_id`, newest first.
    pub async fn messages_in(&self, installed: &Installed, external_id: &str) -> Vec<Value> {
        let conversation = self
            .conversations(installed)
            .await
            .into_iter()
            .find(|conversation| conversation["external_id"] == external_id)
            .unwrap();
        let (status, body) = self
            .setup
            .kit
            .send(
                call(
                    &installed.user,
                    Method::GET,
                    format!(
                        "/v1/workspaces/{}/channel-connections/{}/conversations/{}/messages?limit=100",
                        installed.ws,
                        installed.connection,
                        id(&conversation)
                    ),
                )
                .naming(&installed.org),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["data"].as_array().unwrap().clone()
    }

    /// `installed`'s connection records sending `message` from `run`, `hop`
    /// hops on: what its send does once the provider accepts the post.
    pub async fn sent(&self, installed: &Installed, message: &ChatMessage, run: RunId, hop: u32) {
        let (organization_id, workspace_id) = installed.scope();
        self.setup
            .kit
            .state(Worker::A)
            .channels
            .record_message(
                organization_id,
                workspace_id,
                ChannelConnectionId::parse(&installed.connection).unwrap(),
                &NewMessage {
                    id: ChannelMessageId::generate(),
                    conversation: NewConversation {
                        external_id: message.channel.clone(),
                        kind: ConversationKind::Channel,
                        name: None,
                        is_external: false,
                        identity_is_member: true,
                    },
                    direction: Direction::Outbound,
                    author: None,
                    external_id: message.ts.clone(),
                    thread_key: message.thread_ts.clone().unwrap_or_else(|| message.ts.clone()),
                    text: message.text.clone(),
                    mentions: zupflow_channels::fake::mentions(&message.text),
                    run_id: Some(run),
                    tool_call_id: None,
                    agent_hop: hop,
                    sent_at: self.now(),
                },
                self.now(),
            )
            .await
            .unwrap();
    }

    /// Sets the app's status as stored, for a state no API reaches yet.
    pub async fn set_app_status(&self, org: &str, ws: &str, app: &str, status: &str) {
        let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", self.setup.kit.database().display()))
            .await
            .unwrap();
        let updated =
            sqlx::query("UPDATE channel_apps SET status = ? WHERE organization_id = ? AND workspace_id = ? AND id = ?")
                .bind(status)
                .bind(OrganizationId::parse(org).unwrap().uuid())
                .bind(WorkspaceId::parse(ws).unwrap().uuid())
                .bind(ChannelAppId::parse(app).unwrap().uuid())
                .execute(&pool)
                .await
                .unwrap();
        assert_eq!(updated.rows_affected(), 1);
        pool.close().await;
    }

    /// Every value the database holds, as text where it is text, one per
    /// line: what a test finds nowhere is nowhere.
    pub async fn database_text(&self) -> String {
        let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", self.setup.kit.database().display()))
            .await
            .unwrap();
        let tables: Vec<(String,)> = sqlx::query_as("SELECT name FROM sqlite_master WHERE type = 'table'")
            .fetch_all(&pool)
            .await
            .unwrap();
        let mut text = String::new();
        for (table,) in tables {
            let columns: Vec<(String,)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT name FROM pragma_table_info('{table}')"
            )))
            .fetch_all(&pool)
            .await
            .unwrap();
            for (column,) in columns {
                let values: Vec<(Option<Vec<u8>>,)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                    "SELECT CAST(\"{column}\" AS BLOB) FROM \"{table}\""
                )))
                .fetch_all(&pool)
                .await
                .unwrap();
                for value in values.into_iter().filter_map(|(value,)| value) {
                    text.push_str(&format!("{table}.{column}: "));
                    text.push_str(&String::from_utf8_lossy(&value).replace('\n', " "));
                    text.push('\n');
                }
            }
        }
        pool.close().await;
        text
    }

    /// Approves the call `run` waits on, as the organization's backend would.
    pub async fn approve(&self, run: RunId) {
        let key = self.setup.api_key().await;
        let waiting = self
            .setup
            .calls(run)
            .await
            .into_iter()
            .find(|call| call.state == CallState::AwaitingApproval)
            .unwrap();
        let (status, body) = self.setup.decide(&key, run, &waiting.id.to_string(), "approved").await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    /// The run's `channel_messages.received` events: how each batch came in.
    pub async fn taken_in(&self, run: RunId) -> Vec<(String, usize)> {
        self.setup
            .events(run)
            .await
            .iter()
            .filter(|event| event["type"] == "channel_messages.received")
            .map(|event| {
                (
                    event["data"]["outcome"].as_str().unwrap().to_owned(),
                    event["data"]["message_ids"].as_array().unwrap().len(),
                )
            })
            .collect()
    }
}
