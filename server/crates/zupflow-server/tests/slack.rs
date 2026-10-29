//! Slack end to end, against Slack's Web API faked in process
//! (`zupflow_slack::fake`) behind the same outbound seam the real calls take:
//! a team linked with its configuration token, rotated once per version
//! however many sweeps and workers race; an agent's app made, kept in step
//! with its agent and removed with it; the app installed through hosted
//! Connect with a single-use state, only in its own team and workspace; a
//! disconnect revoking the bot token; a mention read from a signed event,
//! answered by a run and replied to in its thread, and a shared channel's
//! guest waking no one unless the agent allows them. No token or secret is
//! ever answered, queued or logged.

#![allow(clippy::unwrap_used)]

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::{Server, call, code};
use serde_json::{Value, json};
use zupflow_core::channels::ChannelProviderKind;
use zupflow_core::channels::provision::SLACK_SCOPES;
use zupflow_core::channels::registry::ChannelProviders;
use zupflow_core::ids::ChannelAppManagerId;
use zupflow_server::RequestGuard;
use zupflow_slack::fake::{Delivery, FakeSlack, Fault};
use zupflow_slack::{SlackChannelProvider, SlackProvisioner};

const PUBLIC: &str = "https://zupflow.example";
const TEAM: &str = "T0ACME";
const CONFIGURATION_TOKEN: &str = "xoxe.xoxp-1-acme-config-7q2k";
const REFRESH_TOKEN: &str = "xoxe-1-acme-refresh-9f3d";

/// A server whose channels reach Slack through the real calls, over a fake Slack.
async fn slack_server() -> (Server, Arc<FakeSlack>) {
    let mut server = Server::new().await;
    let slack = Arc::new(FakeSlack::new());
    server.state.outbound = slack.clone();
    server.state.public_url = PUBLIC.to_owned();
    restart(&mut server, true);
    (server, slack)
}

/// The same server started again, with the same database and keys, reaching
/// Slack or not (as when its public URL is `https://` or isn't).
fn restart(server: &mut Server, reaches_slack: bool) {
    server.state.channel_providers = if reaches_slack {
        ChannelProviders::none().with(
            Arc::new(SlackProvisioner::new(FakeSlack::api())),
            Arc::new(SlackChannelProvider::new(FakeSlack::api())),
        )
    } else {
        ChannelProviders::none()
    };
    server.app = zupflow_server::app(server.state.clone(), RequestGuard::default());
}

struct Tenant {
    org: String,
    ws: String,
}

async fn tenant(server: &Server, user: &str, slug: &str) -> Tenant {
    let org = server.organization(user, slug).await;
    let ws = server
        .workspace(user, &org, &format!("{slug}-sandbox"), "sandbox")
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    Tenant { org, ws }
}

async fn sandbox(server: &Server, user: &str, org: &str, slug: &str) -> String {
    server.workspace(user, org, slug, "sandbox").await["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn link(server: &Server, t: &Tenant, user: &str, token: &str, refresh: &str) -> (StatusCode, Value) {
    server
        .send(
            call(
                user,
                Method::POST,
                format!("/v1/workspaces/{}/channel-app-managers", t.ws),
            )
            .naming(&t.org)
            .body(json!({
                "provider": "slack", "method": "configuration_token",
                "configuration_token": token, "refresh_token": refresh,
            })),
        )
        .await
}

async fn manager(server: &Server, t: &Tenant, user: &str, id: &str) -> Value {
    let (status, body) = server
        .send(
            call(
                user,
                Method::GET,
                format!("/v1/workspaces/{}/channel-app-managers/{id}", t.ws),
            )
            .naming(&t.org),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"].clone()
}

async fn agent(server: &Server, t: &Tenant, user: &str, name: &str) -> String {
    let (status, body) = server
        .send(
            call(user, Method::POST, format!("/v1/workspaces/{}/agents", t.ws))
                .naming(&t.org)
                .body(json!({ "name": name, "description": "Takes notes.", "instructions": "Be brief." })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["data"]["id"].as_str().unwrap().to_owned()
}

/// Every job the channel worker has now, run, including any a request woke
/// a worker of its own for.
async fn drain(server: &Server) {
    for _ in 0..200 {
        if zupflow_server::channels::work_once(&server.state).await {
            continue;
        }
        let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", server.directory.join("zupflow.sqlite").display()))
            .await
            .unwrap();
        let (running,): (i64,) = sqlx::query_as("SELECT count(*) FROM jobs WHERE state = 'running'")
            .fetch_one(&pool)
            .await
            .unwrap();
        pool.close().await;
        if running == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the channel worker never ran dry");
}

/// An agent's app, made by its job: its ID and its ID at Slack.
async fn made_app(server: &Server, t: &Tenant, user: &str, agent: &str, manager: &str) -> (String, String) {
    let (status, accepted) = server
        .send(
            call(user, Method::POST, format!("/v1/workspaces/{}/channel-apps", t.ws))
                .naming(&t.org)
                .body(json!({ "agent_id": agent, "manager_id": manager })),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    let app = accepted["data"]["id"].as_str().unwrap().to_owned();
    // Creating it kicks a worker of its own; this one waits for it.
    let mut read = Value::Null;
    for _ in 0..100 {
        drain(server).await;
        read = read_app(server, t, user, &app).await;
        if read["status"] != "creating" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(read["status"], "awaiting_install", "{read}");
    (app, read["external_app_id"].as_str().unwrap().to_owned())
}

async fn read_app(server: &Server, t: &Tenant, user: &str, app: &str) -> Value {
    let (status, body) = server
        .send(call(user, Method::GET, format!("/v1/workspaces/{}/channel-apps/{app}", t.ws)).naming(&t.org))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"].clone()
}

/// A hosted Connect session installing `app`: its link's reference.
async fn install_session(server: &Server, t: &Tenant, user: &str, app: &str) -> String {
    let (status, created) = server
        .send(
            call(user, Method::POST, format!("/v1/workspaces/{}/connect-sessions", t.ws))
                .naming(&t.org)
                .body(json!({ "target": { "kind": "channel_app", "app": app }, "return_url": null })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    created["data"]["url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned()
}

/// The page's Continue button: Slack's consent URL and its query.
async fn authorize(server: &Server, reference: &str) -> (url::Url, std::collections::BTreeMap<String, String>) {
    let (status, body) = server
        .send(call(
            "nobody",
            Method::POST,
            format!("/v1/connect/sessions/{reference}/authorize"),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let url = url::Url::parse(body["data"]["authorization_url"].as_str().unwrap()).unwrap();
    let query = url.query_pairs().into_owned().collect();
    (url, query)
}

async fn session(server: &Server, reference: &str) -> Value {
    let (status, body) = server
        .send(call("nobody", Method::GET, format!("/v1/connect/sessions/{reference}")))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"].clone()
}

/// The hosted page, as a browser gets it.
async fn page(server: &Server, reference: &str) -> String {
    let (status, _, body) = server.visit(&format!("/connect/{reference}")).await;
    assert_eq!(status, StatusCode::OK);
    body.as_str().unwrap().to_owned()
}

fn callback(query: &str) -> String {
    format!("/connect/oauth/callback?{query}")
}

/// Every row's bytes the database holds, as text where it is text.
async fn database_text(server: &Server) -> String {
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", server.directory.join("zupflow.sqlite").display()))
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
                text.push_str(&String::from_utf8_lossy(&value));
                text.push('\n');
            }
        }
    }
    pool.close().await;
    text
}

async fn job_payloads(server: &Server) -> Vec<(String, String)> {
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", server.directory.join("zupflow.sqlite").display()))
        .await
        .unwrap();
    let rows: Vec<(String, Vec<u8>)> = sqlx::query_as("SELECT kind, payload FROM jobs")
        .fetch_all(&pool)
        .await
        .unwrap();
    pool.close().await;
    rows.into_iter()
        .map(|(kind, payload)| (kind, String::from_utf8_lossy(&payload).into_owned()))
        .collect()
}

/// Everything logged, at every level.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Link, make, install through hosted Connect, use, disconnect: Slack is
/// called as it documents, the session completes with the connection, and
/// no token or secret is answered, queued, stored in the clear or logged.
#[tokio::test]
async fn installs_an_agents_app_in_its_team_through_hosted_connect() {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    let _default = tracing::subscriber::set_default(subscriber);

    let (server, slack) = slack_server().await;
    let t = tenant(&server, "alice", "acme").await;
    slack.issue_configuration_token(CONFIGURATION_TOKEN, REFRESH_TOKEN, TEAM);
    let mut answered = Vec::new();

    let (status, refused) = link(&server, &t, "alice", "xoxe.xoxp-1-unknown", "xoxe-1-unknown").await;
    assert_eq!(
        (status, code(&refused)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_configuration_token")
    );
    let (status, linked) = link(&server, &t, "alice", CONFIGURATION_TOKEN, REFRESH_TOKEN).await;
    assert_eq!(status, StatusCode::CREATED, "{linked}");
    assert_eq!(linked["data"]["configuration_token"]["last4"], "7q2k");
    let manager = linked["data"]["id"].as_str().unwrap().to_owned();
    answered.push(linked);

    let agent = agent(&server, &t, "alice", "Scribe").await;
    let (app, external) = made_app(&server, &t, "alice", &agent, &manager).await;
    let at_slack = slack.apps().into_iter().find(|made| made.id == external).unwrap();
    let manifest = &at_slack.manifest;
    assert_eq!(manifest["display_information"]["name"], "Scribe");
    assert_eq!(manifest["features"]["bot_user"]["display_name"], "scribe");
    assert_eq!(
        manifest["oauth_config"]["redirect_urls"],
        json!([format!("{PUBLIC}/connect/oauth/callback")])
    );
    let events_url = manifest["settings"]["event_subscriptions"]["request_url"]
        .as_str()
        .unwrap();
    assert!(
        events_url.starts_with(&format!("{PUBLIC}/channels/inbound/")),
        "{events_url}"
    );

    // Made without its events, then subscribed once its signing secret was
    // stored: Slack's challenge verifies against the stored secret.
    let created = slack.calls_to("apps.manifest.create").pop().unwrap();
    assert!(
        serde_json::from_str::<Value>(&created.form["manifest"]).unwrap()["settings"]
            .get("event_subscriptions")
            .is_none()
    );
    let deliveries = slack.deliveries();
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].url, events_url);
    // Slack's challenge, sent to the inbound route as Slack sends it, is
    // answered: it's signed with the secret stored for the app.
    let stored = stored_signing_secret(&server, &t, &app).await;
    assert_eq!(stored, slack.apps()[0].signing_secret);
    let (status, answer) = deliver(&server, &deliveries[0]).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer, json!({ "challenge": format!("challenge-{external}") }));
    let reference = install_session(&server, &t, "alice", &app).await;
    let pending = page(&server, &reference).await;
    assert!(pending.contains("Add Scribe to Slack") && pending.contains("Continue to Slack"));

    let (url, first) = authorize(&server, &reference).await;
    assert_eq!(
        url.as_str().split('?').next(),
        Some("https://slack.fake.example/oauth/v2/authorize")
    );
    assert_eq!(first["client_id"], at_slack.client_id);
    assert_eq!(first["scope"], SLACK_SCOPES.join(","));
    assert_eq!(first["redirect_uri"], format!("{PUBLIC}/connect/oauth/callback"));
    assert!(!first["state"].contains(&reference), "the state is not the link");
    let (_, second) = authorize(&server, &reference).await;
    assert_ne!(first["state"], second["state"], "a fresh state per attempt");

    // Only the newest attempt's state, and never twice; a stale one exchanges nothing.
    let code = slack.allow(&external, TEAM, "Acme");
    let (status, location, body) = server
        .visit(&callback(&format!("code={code}&state={}", first["state"])))
        .await;
    assert_eq!((status, location), (StatusCode::BAD_REQUEST, None));
    assert!(body.as_str().unwrap().contains("invalid_oauth_state"));
    assert!(slack.calls_to("oauth.v2.access").is_empty());

    let arrival = callback(&format!("code={code}&state={}", second["state"]));
    let (status, location, _) = server.visit(&arrival).await;
    assert_eq!(
        (status, location),
        (StatusCode::SEE_OTHER, Some(format!("{PUBLIC}/connect/{reference}")))
    );
    let (status, _, _) = server.visit(&arrival).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a state works once");
    assert_eq!(slack.calls_to("oauth.v2.access").len(), 1);
    let exchange = &slack.calls_to("oauth.v2.access")[0];
    assert_eq!(
        exchange.form["redirect_uri"],
        format!("{PUBLIC}/connect/oauth/callback")
    );

    let completed = session(&server, &reference).await;
    assert_eq!(completed["status"], "completed", "{completed}");
    let connection = completed["target"]["connection"].as_str().unwrap().to_owned();
    assert!(page(&server, &reference).await.contains("Scribe added to Slack"));
    let (status, read) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("/v1/workspaces/{}/channel-connections/{connection}", t.ws),
            )
            .naming(&t.org),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["data"]["team"]["id"], TEAM);
    assert_eq!(read_app(&server, &t, "alice", &app).await["status"], "installed");
    let (_, channels) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("/v1/workspaces/{}/agents/{agent}/channels", t.ws),
            )
            .naming(&t.org),
        )
        .await;
    assert_eq!(channels["data"].as_array().unwrap().len(), 1, "{channels}");
    // The team the manager learned from its first install.
    assert_eq!(manager_team(&server, &t, &manager).await, Some(TEAM.to_owned()));
    answered.extend([completed, read, channels]);

    // Disconnecting revokes the bot token at Slack.
    let (status, _) = server
        .send(
            call(
                "alice",
                Method::DELETE,
                format!("/v1/workspaces/{}/channel-connections/{connection}", t.ws),
            )
            .naming(&t.org),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let install = slack.installs().pop().unwrap();
    assert!(install.revoked, "the bot token was revoked");
    assert_eq!(slack.calls_to("auth.revoke").len(), 1);

    let secrets = [
        CONFIGURATION_TOKEN.to_owned(),
        REFRESH_TOKEN.to_owned(),
        at_slack.client_secret.clone(),
        at_slack.signing_secret.clone(),
        install.token.clone(),
        code.clone(),
    ];
    let answered = Value::Array(answered).to_string();
    let jobs = format!("{:?}", job_payloads(&server).await);
    let stored = database_text(&server).await;
    let logged = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    for secret in &secrets {
        assert!(!answered.contains(secret.as_str()), "answered: {secret}");
        assert!(!jobs.contains(secret.as_str()), "queued: {secret}");
        assert!(!stored.contains(secret.as_str()), "stored in the clear: {secret}");
        assert!(!logged.contains(secret.as_str()), "logged: {secret}");
    }
    assert!(!logged.contains(&reference), "the link isn't logged");
}

/// The app's signing secret as the server stored it, opened as the inbound endpoint opens it.
/// A team-linking link whose provider the organization stopped allowing, or
/// the server stopped reaching, says so on the hosted page instead of asking
/// for tokens; pasting them anyway is refused in words for the person there,
/// the customer's team admin, while the Console is told why. The session
/// stays pending, and the link works again once the provider is back.
#[tokio::test]
async fn tells_the_hosted_page_a_team_link_cant_be_used_before_it_asks_for_tokens() {
    let (mut server, _slack) = slack_server().await;
    let t = tenant(&server, "alice", "acme").await;
    let (status, created) = server
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/workspaces/{}/connect-sessions", t.ws),
            )
            .naming(&t.org)
            .body(json!({ "target": { "kind": "channel_app_manager", "provider": "slack" }, "return_url": null })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let reference = created["data"]["url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned();
    let read_by_api = format!(
        "/v1/workspaces/{}/connect-sessions/{}",
        t.ws,
        created["data"]["id"].as_str().unwrap()
    );
    let choose = |allowed: Value| {
        call(
            "alice",
            Method::PUT,
            format!("/v1/organizations/{}/channel-provider-settings", t.org),
        )
        .naming(&t.org)
        .body(json!({ "allowed_providers": allowed }))
    };
    let paste = || {
        call(
            "nobody",
            Method::POST,
            format!("/v1/connect/sessions/{reference}/credentials"),
        )
        .body(json!({ "values": { "configuration_token": CONFIGURATION_TOKEN, "refresh_token": REFRESH_TOKEN } }))
    };
    const FORM: &str = "data-form=\"credentials\"";
    const UNUSABLE: &str = "This link can&#39;t be used right now";
    assert_eq!(session(&server, &reference).await["unavailable_reason"], Value::Null);
    assert!(page(&server, &reference).await.contains(FORM));

    let (status, body) = server.send(choose(json!([]))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let read = session(&server, &reference).await;
    assert_eq!(
        (&read["status"], &read["unavailable_reason"]),
        (&json!("pending"), &json!("channel_provider_not_allowed"))
    );
    let (_, by_api) = server
        .send(call("alice", Method::GET, read_by_api.clone()).naming(&t.org))
        .await;
    assert_eq!(
        by_api["data"]["unavailable_reason"],
        Value::Null,
        "only the hosted read says it"
    );
    let shown = page(&server, &reference).await;
    assert!(
        shown.contains(UNUSABLE)
            && shown.contains("data-reason=\"channel_provider_not_allowed\"")
            && !shown.contains(FORM),
        "{shown}"
    );
    let (status, refused) = server.send(paste()).await;
    assert_eq!(
        (status, code(&refused)),
        (StatusCode::FORBIDDEN, "channel_provider_not_allowed")
    );
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("Ask the app that sent you here for a new one.") && !message.contains("organization"),
        "{message}"
    );
    let (_, refused) = link(&server, &t, "alice", CONFIGURATION_TOKEN, REFRESH_TOKEN).await;
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("organization's settings"),
        "{refused}"
    );

    // Allowed again, but no longer reached: the same, with the deployment's code.
    server.send(choose(Value::Null)).await;
    restart(&mut server, false);
    assert_eq!(
        session(&server, &reference).await["unavailable_reason"],
        "channel_providers_not_configured"
    );
    assert!(page(&server, &reference).await.contains(UNUSABLE));
    let (status, refused) = server.send(paste()).await;
    assert_eq!(
        (status, code(&refused)),
        (StatusCode::NOT_IMPLEMENTED, "channel_providers_not_configured")
    );
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("Ask the app that sent you here") && !message.contains("ZUPFLOW_PUBLIC_URL"),
        "{message}"
    );

    restart(&mut server, true);
    assert_eq!(session(&server, &reference).await["unavailable_reason"], Value::Null);
    assert!(page(&server, &reference).await.contains(FORM));
}

async fn stored_signing_secret(server: &Server, t: &Tenant, app: &str) -> String {
    use zupflow_core::ids::{ChannelAppId, OrganizationId, WorkspaceId};
    let (org, ws) = (
        OrganizationId::parse(&t.org).unwrap(),
        WorkspaceId::parse(&t.ws).unwrap(),
    );
    let app = ChannelAppId::parse(app).unwrap();
    let record = server.state.channels.app_secrets(org, ws, app).await.unwrap().unwrap();
    assert_eq!(
        record.manifest_digest.as_deref().map(str::len),
        Some(64),
        "subscribed, recorded"
    );
    let sealed = record.secrets.unwrap();
    let context = zupflow_core::keys::EncryptionContext::channel_app_secret(
        org,
        ws,
        zupflow_core::environment::Environment::Sandbox,
        app,
        sealed.version,
    );
    let opened = zupflow_core::keys::open(server.state.keys.as_ref(), &context, &sealed.sealed)
        .await
        .unwrap();
    let secrets: Value = serde_json::from_slice(opened.expose()).unwrap();
    secrets["signing_secret"].as_str().unwrap().to_owned()
}

async fn manager_team(server: &Server, t: &Tenant, manager: &str) -> Option<String> {
    let read = self::manager(server, t, "alice", manager).await;
    read["team"]["id"].as_str().map(str::to_owned)
}

/// Consent refused, a cancelled session, another team: nothing installed,
/// the session failed with why, the page saying so, an unkept token revoked.
#[tokio::test]
async fn refuses_a_denied_consent_a_cancelled_session_and_another_team() {
    let (server, slack) = slack_server().await;
    let t = tenant(&server, "alice", "acme").await;
    slack.issue_configuration_token(CONFIGURATION_TOKEN, REFRESH_TOKEN, TEAM);
    let (_, linked) = link(&server, &t, "alice", CONFIGURATION_TOKEN, REFRESH_TOKEN).await;
    let manager = linked["data"]["id"].as_str().unwrap().to_owned();
    let scribe = agent(&server, &t, "alice", "Scribe").await;
    let (scribe_app, scribe_external) = made_app(&server, &t, "alice", &scribe, &manager).await;

    // Denied: the session fails, nothing is exchanged.
    let reference = install_session(&server, &t, "alice", &scribe_app).await;
    let (_, query) = authorize(&server, &reference).await;
    let (status, location, _) = server
        .visit(&callback(&format!("error=access_denied&state={}", query["state"])))
        .await;
    assert_eq!(
        (status, location),
        (StatusCode::SEE_OTHER, Some(format!("{PUBLIC}/connect/{reference}")))
    );
    let failed = session(&server, &reference).await;
    assert_eq!(
        (failed["status"].as_str(), failed["failure_code"].as_str()),
        (Some("failed"), Some("consent_denied"))
    );
    assert!(page(&server, &reference).await.contains("Access wasn"));

    // Cancelled while at Slack: the state no longer completes anything.
    let reference = install_session(&server, &t, "alice", &scribe_app).await;
    let (_, query) = authorize(&server, &reference).await;
    let (status, _) = server
        .send(
            call(
                "nobody",
                Method::POST,
                format!("/v1/connect/sessions/{reference}/cancel"),
            )
            .idempotent("c-1"),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let code = slack.allow(&scribe_external, TEAM, "Acme");
    let (status, _, _) = server
        .visit(&callback(&format!("code={code}&state={}", query["state"])))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(slack.calls_to("oauth.v2.access").is_empty(), "nothing exchanged");
    assert_eq!(session(&server, &reference).await["status"], "cancelled");

    // The first install teaches the manager its team.
    let reference = install_session(&server, &t, "alice", &scribe_app).await;
    let (_, query) = authorize(&server, &reference).await;
    server
        .visit(&callback(&format!("code={code}&state={}", query["state"])))
        .await;
    assert_eq!(session(&server, &reference).await["status"], "completed");

    // Another agent's app, allowed in another team: refused, its token revoked.
    let quill = agent(&server, &t, "alice", "Quill").await;
    let (quill_app, quill_external) = made_app(&server, &t, "alice", &quill, &manager).await;
    let reference = install_session(&server, &t, "alice", &quill_app).await;
    let (_, query) = authorize(&server, &reference).await;
    let code = slack.allow(&quill_external, "T0OTHER", "Elsewhere");
    let (status, _, _) = server
        .visit(&callback(&format!("code={code}&state={}", query["state"])))
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let failed = session(&server, &reference).await;
    assert_eq!(failed["failure_code"], "team_mismatch", "{failed}");
    assert!(page(&server, &reference).await.contains("different Slack team"));
    let unkept = slack.installs().pop().unwrap();
    assert_eq!((unkept.team_id.as_str(), unkept.revoked), ("T0OTHER", true));
    assert_eq!(
        read_app(&server, &t, "alice", &quill_app).await["status"],
        "awaiting_install"
    );
    let (_, connections) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("/v1/workspaces/{}/channel-connections", t.ws),
            )
            .naming(&t.org),
        )
        .await;
    assert_eq!(connections["data"].as_array().unwrap().len(), 1, "{connections}");
}

/// A callback reaches only the app its session was made for: a code for
/// another workspace's app is refused by Slack against this app's client,
/// and nothing is installed in either; a session can't name another
/// workspace's or organization's app.
#[tokio::test]
async fn keeps_installs_in_their_own_workspace_and_organization() {
    let (server, slack) = slack_server().await;
    let t = tenant(&server, "alice", "acme").await;
    let other = Tenant {
        org: t.org.clone(),
        ws: sandbox(&server, "alice", &t.org, "acme-labs").await,
    };
    let stranger = tenant(&server, "bob", "globex").await;
    let mut externals = Vec::new();
    let mut apps = Vec::new();
    for (i, tenant) in [&t, &other].into_iter().enumerate() {
        let token = format!("xoxe.xoxp-1-ws{i}");
        let refresh = format!("xoxe-1-ws{i}");
        slack.issue_configuration_token(&token, &refresh, TEAM);
        let (_, linked) = link(&server, tenant, "alice", &token, &refresh).await;
        let manager = linked["data"]["id"].as_str().unwrap().to_owned();
        let agent = agent(&server, tenant, "alice", "Scribe").await;
        let (app, external) = made_app(&server, tenant, "alice", &agent, &manager).await;
        apps.push(app);
        externals.push(external);
    }

    // Another workspace's or organization's app can't be a session's target.
    for (who, tenant) in [("alice", &other), ("bob", &stranger)] {
        let (status, body) = server
            .send(
                call(
                    who,
                    Method::POST,
                    format!("/v1/workspaces/{}/connect-sessions", tenant.ws),
                )
                .naming(&tenant.org)
                .body(json!({ "target": { "kind": "channel_app", "app": apps[0] }, "return_url": null })),
            )
            .await;
        assert!(status.is_client_error(), "{who}: {status} {body}");
    }

    // This workspace's session, with a code Slack issued for the other's app.
    let reference = install_session(&server, &t, "alice", &apps[0]).await;
    let (_, query) = authorize(&server, &reference).await;
    let code = slack.allow(&externals[1], TEAM, "Acme");
    let (status, _, _) = server
        .visit(&callback(&format!("code={code}&state={}", query["state"])))
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        session(&server, &reference).await["failure_code"],
        "token_exchange_failed"
    );
    let exchange = slack.calls_to("oauth.v2.access");
    assert_eq!(exchange.len(), 1);
    assert!(slack.installs().is_empty(), "Slack refused another app's code");
    for (tenant, app) in [(&t, &apps[0]), (&other, &apps[1])] {
        assert_eq!(
            read_app(&server, tenant, "alice", app).await["status"],
            "awaiting_install"
        );
        let (_, connections) = server
            .send(
                call(
                    "alice",
                    Method::GET,
                    format!("/v1/workspaces/{}/channel-connections", tenant.ws),
                )
                .naming(&tenant.org),
            )
            .await;
        assert_eq!(connections["data"], json!([]));
    }
}

async fn set_expiry(server: &Server, manager: &str, millis: i64) {
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", server.directory.join("zupflow.sqlite").display()))
        .await
        .unwrap();
    let id = ChannelAppManagerId::parse(manager).unwrap().uuid();
    let updated = sqlx::query("UPDATE channel_app_managers SET token_expires_at = ? WHERE id = ?")
        .bind(millis)
        .bind(id.as_bytes().as_slice())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(updated.rows_affected(), 1);
    pool.close().await;
}

async fn expiry(server: &Server, manager: &str) -> i64 {
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", server.directory.join("zupflow.sqlite").display()))
        .await
        .unwrap();
    let id = ChannelAppManagerId::parse(manager).unwrap().uuid();
    let (millis,): (i64,) = sqlx::query_as("SELECT token_expires_at FROM channel_app_managers WHERE id = ?")
        .bind(id.as_bytes().as_slice())
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    millis
}

fn now_millis() -> i64 {
    zupflow_core::time::Timestamp::now().millis()
}

/// A team's tokens are rotated once per version, however many sweeps and
/// workers race, with the new pair stored before the job ends; not again
/// until the next is due; and a pair Slack refuses leaves the manager
/// `token_expired`.
#[tokio::test]
async fn rotates_a_teams_tokens_once_however_many_workers_race() {
    let (server, slack) = slack_server().await;
    let t = tenant(&server, "alice", "acme").await;
    slack.issue_configuration_token(CONFIGURATION_TOKEN, REFRESH_TOKEN, TEAM);
    let (_, linked) = link(&server, &t, "alice", CONFIGURATION_TOKEN, REFRESH_TOKEN).await;
    let manager = linked["data"]["id"].as_str().unwrap().to_owned();

    // Pasted tokens' expiry isn't known: they're due at once.
    slack.slow("tooling.tokens.rotate", Duration::from_millis(50));
    let sweep = || zupflow_server::channels::rotate_due(&server.state);
    let work = || zupflow_server::channels::work_once(&server.state);
    tokio::join!(sweep(), sweep(), sweep(), sweep());
    let queued = job_payloads(&server).await;
    assert_eq!(queued.len(), 1, "{queued:?}");
    assert_eq!(queued[0].0, "rotate_channel_manager_tokens");
    assert!(!queued[0].1.contains("xoxe"), "{queued:?}");
    tokio::join!(work(), work(), sweep(), work(), sweep(), work());
    drain(&server).await;
    assert_eq!(slack.rotations(), 1, "one refresh token spent once");
    let rotated = self::manager(&server, &t, "alice", &manager).await;
    assert_eq!(rotated["status"], "active", "{rotated}");
    assert!(rotated["token_expires_at"].is_string(), "{rotated}");
    let left = expiry(&server, &manager).await - now_millis();
    assert!((11 * 3_600_000..=12 * 3_600_000 + 60_000).contains(&left), "{left}");
    // The new configuration token is the one used next.
    let agent = agent(&server, &t, "alice", "Scribe").await;
    made_app(&server, &t, "alice", &agent, &manager).await;
    let create = slack.calls_to("apps.manifest.create").pop().unwrap();
    assert_eq!(create.authorization.as_deref(), Some("Bearer xoxe.xoxp-1-rotated-1"));

    // Not due again for hours; then once more; then refused.
    rotate(&server).await;
    assert_eq!(slack.rotations(), 1);
    set_expiry(&server, &manager, now_millis() + 30 * 60_000).await;
    rotate(&server).await;
    assert_eq!(slack.rotations(), 2);
    set_expiry(&server, &manager, now_millis() + 30 * 60_000).await;
    slack.fail_next(
        "tooling.tokens.rotate",
        Fault::Error("invalid_refresh_token".to_owned()),
    );
    rotate(&server).await;
    assert_eq!(
        self::manager(&server, &t, "alice", &manager).await["status"],
        "token_expired"
    );
}

/// One sweep and every job it queued.
async fn rotate(server: &Server) {
    zupflow_server::channels::rotate_due(&server.state).await;
    drain(server).await;
}

/// A rate limit postpones a rotation without spending an attempt or the
/// refresh token, and tokens that lapse unrotated are expired.
#[tokio::test]
async fn waits_out_a_rate_limit_and_expires_lapsed_tokens() {
    let (server, slack) = slack_server().await;
    let t = tenant(&server, "alice", "acme").await;
    slack.issue_configuration_token(CONFIGURATION_TOKEN, REFRESH_TOKEN, TEAM);
    let (_, linked) = link(&server, &t, "alice", CONFIGURATION_TOKEN, REFRESH_TOKEN).await;
    let manager = linked["data"]["id"].as_str().unwrap().to_owned();

    slack.fail_next("tooling.tokens.rotate", Fault::RateLimited { secs: 600 });
    rotate(&server).await;
    assert_eq!(slack.calls_to("tooling.tokens.rotate").len(), 1);
    assert_eq!(slack.rotations(), 0);
    assert_eq!(self::manager(&server, &t, "alice", &manager).await["status"], "active");
    // Postponed, not retried at once.
    rotate(&server).await;
    assert_eq!(slack.calls_to("tooling.tokens.rotate").len(), 1);

    set_expiry(&server, &manager, now_millis() - 1).await;
    zupflow_server::channels::rotate_due(&server.state).await;
    assert_eq!(
        self::manager(&server, &t, "alice", &manager).await["status"],
        "token_expired"
    );
}

/// An agent's new name reaches its app's manifest (after a rate limit,
/// without spending an attempt), and a deleted app is deleted at Slack.
#[tokio::test]
async fn keeps_an_app_in_step_with_its_agent_and_removes_it() {
    let (server, slack) = slack_server().await;
    let t = tenant(&server, "alice", "acme").await;
    slack.issue_configuration_token(CONFIGURATION_TOKEN, REFRESH_TOKEN, TEAM);
    let (_, linked) = link(&server, &t, "alice", CONFIGURATION_TOKEN, REFRESH_TOKEN).await;
    let manager = linked["data"]["id"].as_str().unwrap().to_owned();
    let agent = agent(&server, &t, "alice", "Scribe").await;
    let (app, external) = made_app(&server, &t, "alice", &agent, &manager).await;
    // Made, then subscribed: the one update so far.
    assert_eq!(slack.calls_to("apps.manifest.update").len(), 1);

    slack.fail_next("apps.manifest.update", Fault::RateLimited { secs: 1 });
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::PATCH,
                format!("/v1/workspaces/{}/agents/{agent}", t.ws),
            )
            .naming(&t.org)
            .body(json!({ "name": "Quill" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    drain(&server).await;
    assert_eq!(slack.calls_to("apps.manifest.update").len(), 2, "waiting out the limit");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    drain(&server).await;
    let updates = slack.calls_to("apps.manifest.update");
    assert_eq!(updates.len(), 3);
    assert_eq!(updates[2].form["app_id"], external);
    let at_slack = slack.apps().into_iter().find(|made| made.id == external).unwrap();
    assert_eq!(at_slack.manifest["display_information"]["name"], "Quill");
    assert_eq!(at_slack.manifest["features"]["bot_user"]["display_name"], "quill");

    // Renamed and back before the jobs run: Slack already has that manifest.
    for name in ["Scribe", "Quill"] {
        let (status, body) = server
            .send(
                call(
                    "alice",
                    Method::PATCH,
                    format!("/v1/workspaces/{}/agents/{agent}", t.ws),
                )
                .naming(&t.org)
                .body(json!({ "name": name })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    drain(&server).await;
    assert_eq!(slack.calls_to("apps.manifest.update").len(), 3, "nothing new to send");

    let (status, _) = server
        .send(
            call(
                "alice",
                Method::DELETE,
                format!("/v1/workspaces/{}/channel-apps/{app}", t.ws),
            )
            .naming(&t.org),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    drain(&server).await;
    let deletes = slack.calls_to("apps.manifest.delete");
    assert_eq!(deletes.len(), 1);
    assert_eq!(deletes[0].form["app_id"], external);
    assert!(slack.apps().iter().all(|made| made.deleted));
}

/// Hosted Connect links a team with its pasted tokens, on a page in Slack's
/// words.
#[tokio::test]
async fn links_a_team_from_the_hosted_page() {
    let (server, slack) = slack_server().await;
    let t = tenant(&server, "alice", "acme").await;
    slack.issue_configuration_token(CONFIGURATION_TOKEN, REFRESH_TOKEN, TEAM);
    let (status, created) = server
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/workspaces/{}/connect-sessions", t.ws),
            )
            .naming(&t.org)
            .body(json!({ "target": { "kind": "channel_app_manager", "provider": "slack" }, "return_url": null })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let reference = created["data"]["url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned();
    let shown = page(&server, &reference).await;
    assert!(shown.contains("Link a Slack team") && shown.contains("data-key=\"configuration_token\""));
    assert!(shown.contains("data-key=\"refresh_token\""));

    let submit = |token: &str| {
        call(
            "nobody",
            Method::POST,
            format!("/v1/connect/sessions/{reference}/credentials"),
        )
        .body(json!({
            "values": { "configuration_token": token, "refresh_token": REFRESH_TOKEN },
        }))
    };
    let (status, refused) = server.send(submit("xoxe.xoxp-1-unknown")).await;
    assert_eq!(
        (status, code(&refused)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_configuration_token"),
        "{refused}"
    );
    let (status, linked) = server.send(submit(CONFIGURATION_TOKEN)).await;
    assert_eq!(status, StatusCode::OK, "{linked}");
    assert!(!linked.to_string().contains(CONFIGURATION_TOKEN));
    assert_eq!(session(&server, &reference).await["status"], "completed");
    assert!(page(&server, &reference).await.contains("Slack team linked"));
}

/// The queued jobs, each with the attempts it has used.
async fn queued(server: &Server) -> Vec<(String, i64)> {
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", server.directory.join("zupflow.sqlite").display()))
        .await
        .unwrap();
    let mut rows: Vec<(String, i64)> = sqlx::query_as("SELECT kind, attempts FROM jobs WHERE state = 'queued'")
        .fetch_all(&pool)
        .await
        .unwrap();
    pool.close().await;
    rows.sort();
    rows
}

/// Every queued job due now, as though the time it waits had passed.
async fn time_passes(server: &Server) {
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", server.directory.join("zupflow.sqlite").display()))
        .await
        .unwrap();
    sqlx::query("UPDATE jobs SET run_at = 0 WHERE state = 'queued'")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

/// A server that doesn't reach Slack (restarted without an https public
/// URL) says so at startup, by a count, and keeps a linked team's rotation
/// waiting however often it comes up, spending neither an attempt nor the
/// refresh token; started again reaching Slack, it rotates them.
#[tokio::test]
async fn rotates_a_teams_tokens_once_a_server_reaches_slack_again() {
    let (mut server, slack) = slack_server().await;
    let t = tenant(&server, "alice", "acme").await;
    slack.issue_configuration_token(CONFIGURATION_TOKEN, REFRESH_TOKEN, TEAM);
    let (_, linked) = link(&server, &t, "alice", CONFIGURATION_TOKEN, REFRESH_TOKEN).await;
    let manager = linked["data"]["id"].as_str().unwrap().to_owned();
    assert!(
        zupflow_server::channels::report_unreached(&server.state)
            .await
            .is_empty()
    );

    restart(&mut server, false);
    assert_eq!(
        zupflow_server::channels::report_unreached(&server.state).await,
        vec![(ChannelProviderKind::Slack, 1)]
    );
    for _ in 0..3 {
        rotate(&server).await;
        time_passes(&server).await;
    }
    assert!(slack.calls_to("tooling.tokens.rotate").is_empty());
    assert_eq!(
        queued(&server).await,
        vec![("rotate_channel_manager_tokens".to_owned(), 0)],
        "waiting, with no attempt used"
    );
    assert_eq!(self::manager(&server, &t, "alice", &manager).await["status"], "active");

    restart(&mut server, true);
    assert!(
        zupflow_server::channels::report_unreached(&server.state)
            .await
            .is_empty()
    );
    rotate(&server).await;
    assert_eq!(slack.rotations(), 1);
    let rotated = self::manager(&server, &t, "alice", &manager).await;
    assert_eq!(rotated["status"], "active", "{rotated}");
    assert!(rotated["token_expires_at"].is_string(), "{rotated}");
    assert!(queued(&server).await.is_empty());
}

/// While the server doesn't reach Slack, an app asked for, an agent renamed
/// and an app removed each wait for one that does, using no attempt, and a
/// connection isn't removed with a token nothing here can revoke; started
/// again reaching Slack, each is done.
#[tokio::test]
async fn carries_on_with_apps_once_a_server_reaches_slack_again() {
    let (mut server, slack) = slack_server().await;
    let t = tenant(&server, "alice", "acme").await;
    slack.issue_configuration_token(CONFIGURATION_TOKEN, REFRESH_TOKEN, TEAM);
    let (_, linked) = link(&server, &t, "alice", CONFIGURATION_TOKEN, REFRESH_TOKEN).await;
    let manager = linked["data"]["id"].as_str().unwrap().to_owned();
    // The team's tokens rotated now, so no rotation comes up below.
    rotate(&server).await;
    let scribe = agent(&server, &t, "alice", "Scribe").await;
    let (app, external) = made_app(&server, &t, "alice", &scribe, &manager).await;
    let reference = install_session(&server, &t, "alice", &app).await;
    let (_, query) = authorize(&server, &reference).await;
    let consent = slack.allow(&external, TEAM, "Acme");
    server
        .visit(&callback(&format!("code={consent}&state={}", query["state"])))
        .await;
    let connection = session(&server, &reference).await["target"]["connection"]
        .as_str()
        .unwrap()
        .to_owned();
    let other = agent(&server, &t, "alice", "Herald").await;
    let calls = slack.calls().len();

    restart(&mut server, false);
    let (status, accepted) = server
        .send(
            call("alice", Method::POST, format!("/v1/workspaces/{}/channel-apps", t.ws))
                .naming(&t.org)
                .body(json!({ "agent_id": other, "manager_id": manager })),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    let asked = accepted["data"]["id"].as_str().unwrap().to_owned();
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::PATCH,
                format!("/v1/workspaces/{}/agents/{scribe}", t.ws),
            )
            .naming(&t.org)
            .body(json!({ "name": "Quill" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let disconnect = || {
        call(
            "alice",
            Method::DELETE,
            format!("/v1/workspaces/{}/channel-connections/{connection}", t.ws),
        )
        .naming(&t.org)
    };
    let (status, refused) = server.send(disconnect()).await;
    assert_eq!(
        (status, code(&refused)),
        (StatusCode::NOT_IMPLEMENTED, "channel_providers_not_configured"),
        "{refused}"
    );
    for _ in 0..3 {
        drain(&server).await;
        time_passes(&server).await;
    }
    assert_eq!(slack.calls().len(), calls, "nothing reached Slack");
    assert_eq!(
        queued(&server).await,
        vec![
            ("provision_channel_app".to_owned(), 0),
            ("sync_channel_app".to_owned(), 0),
        ],
        "waiting, with no attempt used"
    );
    assert_eq!(read_app(&server, &t, "alice", &asked).await["status"], "creating");
    let (status, _) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("/v1/workspaces/{}/channel-connections/{connection}", t.ws),
            )
            .naming(&t.org),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "the connection and its token are kept");

    restart(&mut server, true);
    drain(&server).await;
    assert_eq!(
        read_app(&server, &t, "alice", &asked).await["status"],
        "awaiting_install"
    );
    let at_slack = slack.apps().into_iter().find(|made| made.id == external).unwrap();
    assert_eq!(at_slack.manifest["display_information"]["name"], "Quill");
    assert!(queued(&server).await.is_empty());

    // Removed while Slack isn't reached: its token is revoked, and it's deleted there, once it is.
    restart(&mut server, false);
    let (status, _) = server
        .send(
            call(
                "alice",
                Method::DELETE,
                format!("/v1/workspaces/{}/channel-apps/{app}", t.ws),
            )
            .naming(&t.org),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    for _ in 0..3 {
        drain(&server).await;
        time_passes(&server).await;
    }
    assert!(!slack.installs().pop().unwrap().revoked);
    assert!(slack.calls_to("apps.manifest.delete").is_empty());
    assert_eq!(queued(&server).await, vec![("release_channel_app".to_owned(), 0)]);

    restart(&mut server, true);
    drain(&server).await;
    assert!(slack.installs().pop().unwrap().revoked, "its bot token revoked");
    let deletes = slack.calls_to("apps.manifest.delete");
    assert_eq!(deletes.len(), 1);
    assert_eq!(deletes[0].form["app_id"], external);
    assert!(queued(&server).await.is_empty());
}

/// Slack refusing a rotation for a reason other than an invalid refresh
/// token is tried again later: the team stays active.
#[tokio::test]
async fn retries_a_rotation_slack_refuses_for_now() {
    let (server, slack) = slack_server().await;
    let t = tenant(&server, "alice", "acme").await;
    slack.issue_configuration_token(CONFIGURATION_TOKEN, REFRESH_TOKEN, TEAM);
    let (_, linked) = link(&server, &t, "alice", CONFIGURATION_TOKEN, REFRESH_TOKEN).await;
    let manager = linked["data"]["id"].as_str().unwrap().to_owned();
    slack.fail_next("tooling.tokens.rotate", Fault::Error("fatal_error".to_owned()));
    rotate(&server).await;
    assert_eq!(slack.calls_to("tooling.tokens.rotate").len(), 1);
    assert_eq!(self::manager(&server, &t, "alice", &manager).await["status"], "active");
    let retried: Vec<_> = job_payloads(&server)
        .await
        .into_iter()
        .filter(|(kind, _)| kind == "rotate_channel_manager_tokens")
        .collect();
    assert_eq!(retried.len(), 1, "kept for a later attempt");
}

/// A deleted app's bot token is revoked at once, whatever its team's state;
/// the app itself waits for the team's tokens, which keeps the team linked
/// until then, and pasting new ones removes it.
#[tokio::test]
async fn removes_a_deleted_app_once_its_team_has_tokens_again() {
    let (server, slack) = slack_server().await;
    let t = tenant(&server, "alice", "acme").await;
    slack.issue_configuration_token(CONFIGURATION_TOKEN, REFRESH_TOKEN, TEAM);
    let (_, linked) = link(&server, &t, "alice", CONFIGURATION_TOKEN, REFRESH_TOKEN).await;
    let manager = linked["data"]["id"].as_str().unwrap().to_owned();
    let agent = agent(&server, &t, "alice", "Scribe").await;
    let (app, external) = made_app(&server, &t, "alice", &agent, &manager).await;
    let reference = install_session(&server, &t, "alice", &app).await;
    let (_, query) = authorize(&server, &reference).await;
    let consent = slack.allow(&external, TEAM, "Acme");
    server
        .visit(&callback(&format!("code={consent}&state={}", query["state"])))
        .await;
    assert_eq!(session(&server, &reference).await["status"], "completed");

    // The team's tokens are refused: expired.
    slack.fail_next(
        "tooling.tokens.rotate",
        Fault::Error("invalid_refresh_token".to_owned()),
    );
    rotate(&server).await;
    assert_eq!(
        self::manager(&server, &t, "alice", &manager).await["status"],
        "token_expired"
    );

    let (status, _) = server
        .send(
            call(
                "alice",
                Method::DELETE,
                format!("/v1/workspaces/{}/channel-apps/{app}", t.ws),
            )
            .naming(&t.org),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    drain(&server).await;
    let install = slack.installs().pop().unwrap();
    assert!(install.revoked, "its bot token is revoked without its team's tokens");
    assert!(
        slack.calls_to("apps.manifest.delete").is_empty(),
        "waiting for the team's tokens"
    );
    let unlink = || {
        call(
            "alice",
            Method::DELETE,
            format!("/v1/workspaces/{}/channel-app-managers/{manager}", t.ws),
        )
        .naming(&t.org)
    };
    let (status, refused) = server.send(unlink()).await;
    assert_eq!((status, code(&refused)), (StatusCode::CONFLICT, "in_use"), "{refused}");

    // New tokens pasted: the removal runs again, and the team can go.
    slack.issue_configuration_token("xoxe.xoxp-1-acme-again", "xoxe-1-acme-again", TEAM);
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::PATCH,
                format!("/v1/workspaces/{}/channel-app-managers/{manager}", t.ws),
            )
            .naming(&t.org)
            .body(json!({ "configuration_token": "xoxe.xoxp-1-acme-again", "refresh_token": "xoxe-1-acme-again" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    drain(&server).await;
    let deletes = slack.calls_to("apps.manifest.delete");
    assert_eq!(deletes.len(), 1);
    assert_eq!(deletes[0].form["app_id"], external);
    let (status, _) = server.send(unlink()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// Making an app is two steps, each safe to retry: when the second (its
/// events) has to wait, the job's next run only subscribes them, and never
/// makes a second app.
#[tokio::test]
async fn subscribes_a_made_apps_events_on_a_later_run() {
    let (server, slack) = slack_server().await;
    let t = tenant(&server, "alice", "acme").await;
    slack.issue_configuration_token(CONFIGURATION_TOKEN, REFRESH_TOKEN, TEAM);
    let (_, linked) = link(&server, &t, "alice", CONFIGURATION_TOKEN, REFRESH_TOKEN).await;
    let manager = linked["data"]["id"].as_str().unwrap().to_owned();
    let agent = agent(&server, &t, "alice", "Scribe").await;
    slack.fail_next("apps.manifest.update", Fault::RateLimited { secs: 1 });
    let (app, external) = made_app(&server, &t, "alice", &agent, &manager).await;
    drain(&server).await;
    assert!(slack.deliveries().is_empty(), "not subscribed yet");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    drain(&server).await;
    assert_eq!(slack.calls_to("apps.manifest.create").len(), 1, "made once");
    assert_eq!(slack.calls_to("apps.manifest.update").len(), 2);
    let deliveries = slack.deliveries();
    assert_eq!(deliveries.len(), 1);
    assert!(deliveries[0].url.starts_with(&format!("{PUBLIC}/channels/inbound/")));
    assert_eq!(slack.apps()[0].id, external);
    stored_signing_secret(&server, &t, &app).await;
    assert!(job_payloads(&server).await.is_empty(), "nothing left to do");
}

// --- Messages and replies ---------------------------------------------------

/// Sends a delivery Slack made to its events URL, as Slack sends it.
async fn deliver(server: &Server, delivery: &zupflow_slack::fake::Delivery) -> (StatusCode, Value) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let path = delivery.url.strip_prefix(PUBLIC).unwrap();
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("Content-Type", "application/json")
        .header("X-Slack-Request-Timestamp", &delivery.timestamp)
        .header("X-Slack-Signature", &delivery.signature)
        .body(axum::body::Body::from(delivery.body.clone()))
        .unwrap();
    let response = server.app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// An agent's app made and installed in [`TEAM`] through hosted Connect,
/// with a model to answer with: where Slack sends its events, what they're
/// signed with, and the bot user it answers as.
struct Answering {
    user: &'static str,
    team: &'static str,
    t: Tenant,
    agent: String,
    connection: String,
    events_url: String,
    signing_secret: String,
    bot_user: String,
}

async fn answering(server: &Server, slack: &FakeSlack) -> Answering {
    answering_as(server, slack, "alice", "acme", TEAM).await
}

/// [`answering`], for `user`'s own organization `slug`, in its own team.
async fn answering_as(
    server: &Server,
    slack: &FakeSlack,
    user: &'static str,
    slug: &str,
    team: &'static str,
) -> Answering {
    let t = tenant(server, user, slug).await;
    server.default_model(user, &t.org, "sandbox").await;
    let (configuration_token, refresh_token) = if team == TEAM {
        (CONFIGURATION_TOKEN.to_owned(), REFRESH_TOKEN.to_owned())
    } else {
        (format!("xoxe.xoxp-1-{slug}-config"), format!("xoxe-1-{slug}-refresh"))
    };
    slack.issue_configuration_token(&configuration_token, &refresh_token, team);
    let (status, linked) = link(server, &t, user, &configuration_token, &refresh_token).await;
    assert_eq!(status, StatusCode::CREATED, "{linked}");
    let manager = linked["data"]["id"].as_str().unwrap().to_owned();
    let agent = agent(server, &t, user, "Scribe").await;
    let (app, external) = made_app(server, &t, user, &agent, &manager).await;
    let reference = install_session(server, &t, user, &app).await;
    let (_, query) = authorize(server, &reference).await;
    let code = slack.allow(&external, team, slug);
    let (status, _, _) = server
        .visit(&callback(&format!("code={code}&state={}", query["state"])))
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, connections) = server
        .send(
            call(
                user,
                Method::GET,
                format!("/v1/workspaces/{}/channel-connections", t.ws),
            )
            .naming(&t.org),
        )
        .await;
    let connection = connections["data"][0]["id"].as_str().unwrap().to_owned();
    let made = slack.apps().into_iter().find(|made| made.id == external).unwrap();
    let events_url = made.manifest["settings"]["event_subscriptions"]["request_url"]
        .as_str()
        .unwrap()
        .to_owned();
    let bot_user = slack.installs().pop().unwrap().bot_user_id;
    Answering {
        user,
        team,
        t,
        agent,
        connection,
        events_url,
        signing_secret: made.signing_secret,
        bot_user,
    }
}

fn now_seconds() -> i64 {
    now_millis() / 1000
}

/// A message event as Slack delivers it to the app, signed now.
fn message_event(to: &Answering, event_id: &str, event: Value, shared: bool) -> zupflow_slack::fake::Delivery {
    let body = json!({
        "type": "event_callback", "team_id": to.team, "api_app_id": "A0FAKE", "event_id": event_id,
        "event_time": now_seconds(), "is_ext_shared_channel": shared, "event": event,
    })
    .to_string();
    let timestamp = now_seconds().to_string();
    Delivery {
        url: to.events_url.clone(),
        signature: zupflow_slack::events::sign(to.signing_secret.as_bytes(), &timestamp, body.as_bytes()),
        timestamp,
        body,
    }
}

/// Someone posts in `channel`; Slack delivers it to the app.
async fn posted(server: &Server, to: &Answering, event_id: &str, event: Value, shared: bool) {
    let (status, answer) = deliver(server, &message_event(to, event_id, event, shared)).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
}

/// The inbound jobs, the runs they started and the replies they made,
/// worked as far as they go.
async fn answer_all(server: &Server) {
    while zupflow_server::channel_inbound::work_once(&server.state).await {}
    while zupflow_server::run_engine::work_once(&server.state).await {}
    while zupflow_server::channel_reply::work_once(&server.state).await {}
}

async fn channel_runs(server: &Server, to: &Answering) -> Vec<Value> {
    let (status, body) = server
        .send(
            call(
                to.user,
                Method::GET,
                format!("/v1/workspaces/{}/runs?agent_id={}&limit=100", to.t.ws, to.agent),
            )
            .naming(&to.t.org),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|run| run["trigger"] == "channel")
        .cloned()
        .collect()
}

/// A mention, end to end in Slack's terms: Slack's signed `event_callback`
/// reaches the inbound route; the inbound job reads its author's name once
/// with `users.info`; the run reads it by that name; its answer is posted
/// in the message's thread with `chat.postMessage`, carrying the run's ID,
/// and recorded once, though Slack echoes it back and the job runs again.
#[tokio::test]
async fn answers_a_mention_in_its_thread_through_slack() {
    let (server, slack) = slack_server().await;
    let to = answering(&server, &slack).await;
    slack.add_user(json!({
        "id": "U0ALICE", "name": "alice", "deleted": false, "is_bot": false,
        "profile": { "display_name": "Alice", "real_name": "Alice Moreau", "email": "alice@acme.example" },
    }));
    let ts = format!("{}.000100", now_seconds());
    let mention = json!({
        "type": "message", "user": "U0ALICE", "team": TEAM, "channel": "C0GENERAL", "channel_type": "channel",
        "text": format!("<@{}> what's on today?", to.bot_user), "ts": ts, "event_ts": ts,
    });
    posted(&server, &to, "Ev0MENTION", mention.clone(), false).await;
    // Slack's `app_mention` for the same message is recorded and nothing more.
    let mut app_mention = mention.clone();
    app_mention["type"] = json!("app_mention");
    posted(&server, &to, "Ev0APPMENTION", app_mention, false).await;
    answer_all(&server).await;

    let runs = channel_runs(&server, &to).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    let run = &runs[0];
    assert_eq!(run["status"], "completed", "{run}");
    let input = run["input"].as_str().unwrap();
    assert!(input.contains("[Alice] @"), "read by the name Slack gave: {input}");
    assert!(input.contains("what's on today?"), "{input}");
    let asked = slack.calls_to("users.info");
    assert_eq!(asked.len(), 1, "the author, once; never the agent itself");
    assert_eq!(asked[0].form["user"], "U0ALICE");

    let replies: Vec<_> = slack
        .messages()
        .into_iter()
        .filter(|message| message.channel == "C0GENERAL")
        .collect();
    assert_eq!(replies.len(), 1);
    let reply = &replies[0];
    assert_eq!(reply.thread_ts.as_deref(), Some(ts.as_str()), "in the message's thread");
    assert_eq!(reply.user, to.bot_user);
    assert_eq!(reply.text, run["output"].as_str().unwrap());
    assert_eq!(
        reply.metadata.as_ref().unwrap()["event_payload"]["invocation_id"],
        run["id"]
    );
    let post = slack.calls_to("chat.postMessage").pop().unwrap();
    assert!(
        post.authorization
            .as_deref()
            .is_some_and(|value| value.starts_with("Bearer xoxb-"))
    );

    // Slack echoes the app's own post: recorded as the one sent, once, and
    // it wakes no one.
    posted(
        &server,
        &to,
        "Ev0ECHO",
        json!({
            "type": "message", "subtype": "bot_message", "user": to.bot_user, "bot_id": format!("B{}", to.bot_user),
            "team": TEAM, "channel": "C0GENERAL", "channel_type": "channel", "text": reply.text,
            "ts": reply.ts, "thread_ts": ts, "event_ts": reply.ts,
        }),
        false,
    )
    .await;
    answer_all(&server).await;
    assert_eq!(channel_runs(&server, &to).await.len(), 1);
    assert_eq!(slack.calls_to("chat.postMessage").len(), 1);
    let (_, conversations) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!(
                    "/v1/workspaces/{}/channel-connections/{}/conversations",
                    to.t.ws, to.connection
                ),
            )
            .naming(&to.t.org),
        )
        .await;
    let conversation = conversations["data"][0]["id"].as_str().unwrap();
    let (_, messages) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!(
                    "/v1/workspaces/{}/channel-connections/{}/conversations/{conversation}/messages",
                    to.t.ws, to.connection
                ),
            )
            .naming(&to.t.org),
        )
        .await;
    let directions: Vec<&str> = messages["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["direction"].as_str().unwrap())
        .collect();
    assert_eq!(directions, ["outbound", "inbound"], "newest first: {messages}");
    // A name read once isn't read again.
    let ts = format!("{}.000200", now_seconds());
    posted(
        &server,
        &to,
        "Ev0AGAIN",
        json!({
            "type": "message", "user": "U0ALICE", "team": TEAM, "channel": "C0GENERAL", "channel_type": "channel",
            "text": format!("<@{}> and tomorrow?", to.bot_user), "ts": ts, "event_ts": ts,
        }),
        false,
    )
    .await;
    answer_all(&server).await;
    assert_eq!(slack.calls_to("users.info").len(), 1);
    assert_eq!(slack.calls_to("chat.postMessage").len(), 2);
}

/// Slack's join of `user` to a channel at `at` (seconds), signed now.
fn joined_event(to: &Answering, event_id: &str, channel: &str, user: &str, at: i64) -> Delivery {
    let ts = format!("{at}.000900");
    message_event(
        to,
        event_id,
        json!({
            "type": "member_joined_channel", "user": user, "channel": channel, "channel_type": "C",
            "team": to.team, "inviter": "U0ALICE", "event_ts": ts,
        }),
        false,
    )
}

/// Slack sends an app nothing from a channel it isn't in, so the mention
/// that gets it added there is never delivered. Its own join reads the
/// channel's history from ten minutes before, one page of 15, and answers
/// that mention once, in its thread: never a message older than that,
/// one that doesn't mention it, nor a join's own notice; never again on a
/// second join, or when the mention is delivered after all; and never for
/// someone else's join. Another organization's connection, linked to its
/// own team, reads nothing and answers nothing.
#[tokio::test]
async fn answers_the_mention_that_added_it_to_a_channel_once() {
    let (server, slack) = slack_server().await;
    let to = answering(&server, &slack).await;
    let other = answering_as(&server, &slack, "bob", "globex", "T0GLOBEX").await;
    slack.add_user(json!({
        "id": "U0ALICE", "name": "alice", "deleted": false, "is_bot": false,
        "profile": { "display_name": "Alice", "real_name": "Alice Moreau", "email": "alice@acme.example" },
    }));
    let now = now_seconds();
    let mention = format!("{}.000100", now - 60);
    let bot = &to.bot_user;
    slack.add_message("C0NEW", &mention, "U0ALICE", &format!("<@{bot}> let's talk?"), None);
    slack.add_message(
        "C0NEW",
        &format!("{}.000100", now - 11 * 60),
        "U0ALICE",
        &format!("<@{bot}> anyone?"),
        None,
    );
    slack.add_message(
        "C0NEW",
        &format!("{}.000100", now - 30),
        "U0ALICE",
        "<@U0CAROL> not you",
        None,
    );
    slack.add_message(
        "C0NEW",
        &format!("{}.000200", now - 1),
        bot,
        &format!("<@{bot}> has joined the channel"),
        Some("channel_join"),
    );

    // Someone else joining reads nothing.
    let (status, _) = deliver(&server, &joined_event(&to, "Ev0ALICEJOINS", "C0NEW", "U0ALICE", now)).await;
    assert_eq!(status, StatusCode::OK);
    answer_all(&server).await;
    assert!(slack.calls_to("conversations.history").is_empty());

    // A busy channel: a page's worth said after the join, before the
    // catch-up reads, never pushes out what was said before it.
    for seq in 0..16 {
        slack.add_message(
            "C0NEW",
            &format!("{now}.{:06}", 1000 + seq),
            "U0ALICE",
            &format!("<@{bot}> later {seq}"),
            None,
        );
    }
    let (status, answer) = deliver(&server, &joined_event(&to, "Ev0JOINED", "C0NEW", bot, now)).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    answer_all(&server).await;
    let read = slack.calls_to("conversations.history");
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].form["channel"], "C0NEW");
    assert_eq!(read[0].form["limit"], "15");
    assert_eq!(
        read[0].form["latest"],
        format!("{now}.000000"),
        "up to the join, to the millisecond"
    );
    assert_eq!(
        read[0].form["oldest"]
            .split('.')
            .next()
            .unwrap()
            .parse::<i64>()
            .unwrap(),
        now - 10 * 60,
        "ten minutes before the join"
    );
    let install = slack
        .installs()
        .into_iter()
        .find(|install| install.bot_user_id == *bot)
        .unwrap();
    assert_eq!(
        read[0].authorization.as_deref(),
        Some(format!("Bearer {}", install.token).as_str())
    );

    let runs = channel_runs(&server, &to).await;
    assert_eq!(runs.len(), 1, "the mention, once: {runs:?}");
    assert_eq!(runs[0]["status"], "completed", "{}", runs[0]);
    assert!(runs[0]["input"].as_str().unwrap().contains("let's talk?"));
    let replies: Vec<_> = slack
        .messages()
        .into_iter()
        .filter(|message| message.channel == "C0NEW" && !message.by_person)
        .collect();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].thread_ts.as_deref(), Some(mention.as_str()), "in its thread");
    // Only what was said to it is kept: nothing else of the channel's history.
    let pool = database(&server).await;
    let kept: Vec<(String, String)> = sqlx::query_as(
        "SELECT g.direction, g.external_id FROM channel_messages g JOIN channel_conversations c \
         ON c.organization_id = g.organization_id AND c.workspace_id = g.workspace_id AND c.id = g.conversation_id \
         WHERE c.external_id = 'C0NEW' ORDER BY g.external_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        kept,
        [
            ("inbound".to_owned(), mention.clone()),
            ("outbound".to_owned(), replies[0].ts.clone())
        ]
    );

    // A second join reads again, and answers nothing again; nor does the
    // mention's own delivery, should Slack send it after all.
    let (status, _) = deliver(&server, &joined_event(&to, "Ev0JOINEDAGAIN", "C0NEW", bot, now)).await;
    assert_eq!(status, StatusCode::OK);
    posted(
        &server,
        &to,
        "Ev0LATE",
        json!({
            "type": "message", "user": "U0ALICE", "team": TEAM, "channel": "C0NEW", "channel_type": "channel",
            "text": format!("<@{bot}> let's talk?"), "ts": mention, "event_ts": mention,
        }),
        false,
    )
    .await;
    answer_all(&server).await;
    assert_eq!(slack.calls_to("conversations.history").len(), 2);
    assert_eq!(channel_runs(&server, &to).await.len(), 1);
    assert_eq!(slack.calls_to("chat.postMessage").len(), 1);

    // The other organization's connection: its own join of the same
    // channel ID, in its own team, reads with its own token only, and
    // nothing of the first one's reaches it.
    assert!(channel_runs(&server, &other).await.is_empty());
    let (status, _) = deliver(
        &server,
        &joined_event(&other, "Ev0OTHERJOINED", "C0NEW", &other.bot_user, now),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    answer_all(&server).await;
    let other_install = slack
        .installs()
        .into_iter()
        .find(|install| install.bot_user_id == other.bot_user)
        .unwrap();
    let read = slack.calls_to("conversations.history");
    assert_eq!(read.len(), 3);
    assert_eq!(
        read[2].authorization.as_deref(),
        Some(format!("Bearer {}", other_install.token).as_str())
    );
    assert!(
        channel_runs(&server, &other).await.is_empty(),
        "nothing mentions its agent"
    );
    assert_eq!(channel_runs(&server, &to).await.len(), 1);
    // A join delivered to the first app but naming the other's team is
    // never the first connection's.
    let crossed = message_event(
        &to,
        "Ev0CROSSED",
        json!({
            "type": "member_joined_channel", "user": bot, "channel": "C0ELSEWHERE", "channel_type": "C",
            "team": other.team, "event_ts": format!("{now}.000950"),
        }),
        false,
    );
    let body: Value = serde_json::from_str(&crossed.body).unwrap();
    let mut body = body;
    body["team_id"] = json!(other.team);
    let body = body.to_string();
    let crossed = Delivery {
        signature: zupflow_slack::events::sign(to.signing_secret.as_bytes(), &crossed.timestamp, body.as_bytes()),
        body,
        ..crossed
    };
    let (status, _) = deliver(&server, &crossed).await;
    assert_eq!(status, StatusCode::OK);
    answer_all(&server).await;
    assert_eq!(
        slack.calls_to("conversations.history").len(),
        3,
        "another team's join reads nothing"
    );
}

/// A rate limit on the history read waits as long as Slack asks, without
/// using an attempt, and the mention is answered once Slack answers.
#[tokio::test]
async fn waits_out_a_rate_limit_before_catching_up() {
    let (server, slack) = slack_server().await;
    let to = answering(&server, &slack).await;
    let now = now_seconds();
    let mention = format!("{}.000100", now - 5);
    slack.add_message(
        "C0NEW",
        &mention,
        "U0ALICE",
        &format!("<@{}> hello?", to.bot_user),
        None,
    );
    slack.fail_next("conversations.history", Fault::RateLimited { secs: 60 });
    let (status, _) = deliver(&server, &joined_event(&to, "Ev0JOINED", "C0NEW", &to.bot_user, now)).await;
    assert_eq!(status, StatusCode::OK);
    answer_all(&server).await;
    assert!(channel_runs(&server, &to).await.is_empty(), "waiting");
    let pool = database(&server).await;
    let (state, attempts, run_at): (String, i64, i64) =
        sqlx::query_as("SELECT state, attempts, run_at FROM jobs WHERE kind = 'channel_catch_up'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (state.as_str(), attempts),
        ("queued", 0),
        "a rate limit uses no attempt"
    );
    assert!(run_at >= (now + 55) * 1000, "as long as Slack asked");
    sqlx::query("UPDATE jobs SET run_at = 0 WHERE kind = 'channel_catch_up'")
        .execute(&pool)
        .await
        .unwrap();
    answer_all(&server).await;
    assert_eq!(channel_runs(&server, &to).await.len(), 1);
}

/// A reply whose answer was lost is found by its run's ID, never posted
/// twice; a rate limit waits without an attempt.
#[tokio::test]
async fn posts_a_reply_once_when_slack_loses_its_answer() {
    let (server, slack) = slack_server().await;
    let to = answering(&server, &slack).await;
    let ts = format!("{}.000100", now_seconds());
    posted(
        &server,
        &to,
        "Ev0LOST",
        json!({
            "type": "message", "user": "U0ALICE", "team": TEAM, "channel": "C0GENERAL", "channel_type": "channel",
            "text": format!("<@{}> hello", to.bot_user), "ts": ts, "event_ts": ts,
        }),
        false,
    )
    .await;
    slack.fail_next("chat.postMessage", Fault::LoseAnswer);
    answer_all(&server).await;
    assert_eq!(slack.messages().len(), 1, "posted, though its answer was lost");
    // The retry comes due: it looks before it posts.
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", server.directory.join("zupflow.sqlite").display()))
        .await
        .unwrap();
    sqlx::query("UPDATE jobs SET run_at = 0 WHERE kind = 'channel_reply'")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert!(zupflow_server::channel_reply::work_once(&server.state).await);
    assert_eq!(slack.messages().len(), 1, "found, not posted again");
    assert_eq!(slack.calls_to("chat.postMessage").len(), 1);
    assert_eq!(slack.calls_to("conversations.replies").len(), 1);
    let run = &channel_runs(&server, &to).await[0];
    assert_eq!(
        slack.messages()[0].metadata.as_ref().unwrap()["event_payload"]["invocation_id"],
        run["id"]
    );
    assert!(!zupflow_server::channel_reply::work_once(&server.state).await, "done");
}

/// Slack Connect: a guest of another organization's team, in a channel
/// shared with it, wakes the agent only once its channel allows people of
/// other organizations; a person of its own team in the same channel does.
#[tokio::test]
async fn wakes_no_one_for_a_shared_channels_guest_unless_the_agent_allows_them() {
    let (server, slack) = slack_server().await;
    let to = answering(&server, &slack).await;
    let guest = |event_id: &str, seq: u32| {
        let ts = format!("{}.{seq:06}", now_seconds());
        (
            event_id.to_owned(),
            json!({
                "type": "message", "user": "U0GUEST", "team": "T0OTHERORG", "user_team": "T0OTHERORG",
                "source_team": "T0OTHERORG", "channel": "C0SHARED", "channel_type": "channel",
                "text": format!("<@{}> can you help?", to.bot_user), "ts": ts, "event_ts": ts,
            }),
        )
    };
    let (event_id, event) = guest("Ev0GUEST1", 100);
    posted(&server, &to, &event_id, event, true).await;
    answer_all(&server).await;
    assert!(
        channel_runs(&server, &to).await.is_empty(),
        "no wake without allow_external"
    );
    assert!(slack.calls_to("chat.postMessage").is_empty());
    assert!(
        slack.calls_to("users.info").is_empty(),
        "no one is looked up for a message no agent answers"
    );

    // Someone of the app's own team, in the same shared channel, does.
    let ts = format!("{}.000200", now_seconds());
    posted(
        &server,
        &to,
        "Ev0MEMBER",
        json!({
            "type": "message", "user": "U0ALICE", "team": TEAM, "user_team": TEAM, "channel": "C0SHARED",
            "channel_type": "channel", "text": format!("<@{}> and for me?", to.bot_user), "ts": ts, "event_ts": ts,
        }),
        true,
    )
    .await;
    answer_all(&server).await;
    assert_eq!(channel_runs(&server, &to).await.len(), 1);

    // Once the agent allows people of other organizations, the guest does too.
    let channels = format!("/v1/workspaces/{}/agents/{}/channels", to.t.ws, to.agent);
    let (_, listed) = server
        .send(call("alice", Method::GET, channels.clone()).naming(&to.t.org))
        .await;
    let mut settings = listed["data"][0].clone();
    settings["allow_external"] = json!(true);
    let (status, saved) = server
        .send(
            call("alice", Method::PUT, format!("{channels}/{}", to.connection))
                .naming(&to.t.org)
                .body(settings),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let (event_id, event) = guest("Ev0GUEST2", 300);
    posted(&server, &to, &event_id, event, true).await;
    answer_all(&server).await;
    let runs = channel_runs(&server, &to).await;
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_eq!(slack.calls_to("chat.postMessage").len(), 2);
    let asked: Vec<String> = slack
        .calls_to("users.info")
        .iter()
        .map(|call| call.form["user"].clone())
        .collect();
    assert_eq!(asked, ["U0ALICE", "U0GUEST"], "each once it wakes the agent");
}

/// The server's database, for what no API shows.
async fn database(server: &Server) -> sqlx::SqlitePool {
    sqlx::SqlitePool::connect(&format!("sqlite:{}", server.directory.join("zupflow.sqlite").display()))
        .await
        .unwrap()
}

/// The reply job, as stored: its state, attempts and why it died.
async fn reply_job(pool: &sqlx::SqlitePool) -> (String, i64, Option<String>) {
    sqlx::query_as(
        "SELECT state, attempts, reason FROM jobs WHERE kind = 'channel_reply' ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// What the connection recorded posting: each outbound message's run and
/// hops.
async fn outbound(pool: &sqlx::SqlitePool) -> Vec<(Option<String>, i64)> {
    sqlx::query_as(
        "SELECT CASE WHEN run_id IS NOT NULL THEN lower(hex(run_id)) END, agent_hop FROM channel_messages WHERE direction = 'outbound' \
         ORDER BY created_at, id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

fn run_uuid(run: &Value) -> String {
    let id = run["id"].as_str().unwrap();
    zupflow_core::ids::RunId::parse(id).unwrap().uuid().simple().to_string()
}

/// Asked to wait, the reply waits without using an attempt, then posts,
/// recorded with its run and one hop more than the message it answers; a
/// day after its run ended, it stops waiting and gives up.
#[tokio::test]
async fn waits_out_a_rate_limit_on_a_reply_for_a_day_at_most() {
    let (server, slack) = slack_server().await;
    let to = answering(&server, &slack).await;
    let pool = database(&server).await;
    let mention = |seq: u32| {
        let ts = format!("{}.{seq:06}", now_seconds());
        json!({
            "type": "message", "user": "U0ALICE", "team": TEAM, "channel": "C0GENERAL", "channel_type": "channel",
            "text": format!("<@{}> hello", to.bot_user), "ts": ts, "event_ts": ts,
        })
    };
    posted(&server, &to, "Ev0LIMITED", mention(100), false).await;
    slack.fail_next("chat.postMessage", Fault::RateLimited { secs: 30 });
    answer_all(&server).await;
    assert!(slack.messages().is_empty());
    assert_eq!(
        reply_job(&pool).await,
        ("queued".to_owned(), 0, None),
        "no attempt used"
    );
    sqlx::query("UPDATE jobs SET run_at = 0 WHERE kind = 'channel_reply'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(zupflow_server::channel_reply::work_once(&server.state).await);
    assert_eq!(slack.messages().len(), 1);
    assert!(
        slack.calls_to("conversations.replies").is_empty(),
        "nothing to look for"
    );
    let run = &channel_runs(&server, &to).await[0];
    assert_eq!(
        slack.messages()[0].metadata.as_ref().unwrap()["event_payload"]["invocation_id"],
        run["id"]
    );
    assert_eq!(outbound(&pool).await, [(Some(run_uuid(run)), 1)]);

    // A day on, still limited: given up.
    posted(&server, &to, "Ev0LATE", mention(200), false).await;
    slack.fail_next("chat.postMessage", Fault::RateLimited { secs: 30 });
    answer_all(&server).await;
    let day_ago = now_millis() - 24 * 60 * 60 * 1000 - 1000;
    sqlx::query("UPDATE runs SET finished_at = ? WHERE channel_session_id IS NOT NULL AND finished_at > ?")
        .bind(day_ago)
        .bind(day_ago)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE jobs SET run_at = 0 WHERE kind = 'channel_reply' AND state = 'queued'")
        .execute(&pool)
        .await
        .unwrap();
    slack.fail_next("chat.postMessage", Fault::RateLimited { secs: 30 });
    assert!(zupflow_server::channel_reply::work_once(&server.state).await);
    assert_eq!(
        reply_job(&pool).await,
        ("dead".to_owned(), 1, Some("rate_limited".to_owned())),
        "only the claim that gave up counted"
    );
    assert_eq!(slack.messages().len(), 1, "never posted");
    assert!(!zupflow_server::channel_reply::work_once(&server.state).await);
    pool.close().await;
}

/// Slack's echo of a post whose answer was lost arrives before the reply
/// job records it: the retry finds the post, and fills in the echo's
/// record with its run and hops, never posting again.
#[tokio::test]
async fn fills_in_the_echo_of_a_reply_recorded_before_it() {
    let (server, slack) = slack_server().await;
    let to = answering(&server, &slack).await;
    let pool = database(&server).await;
    let ts = format!("{}.000100", now_seconds());
    posted(
        &server,
        &to,
        "Ev0ASKED",
        json!({
            "type": "message", "user": "U0ALICE", "team": TEAM, "channel": "C0GENERAL", "channel_type": "channel",
            "text": format!("<@{}> hello", to.bot_user), "ts": ts, "event_ts": ts,
        }),
        false,
    )
    .await;
    slack.fail_next("chat.postMessage", Fault::LoseAnswer);
    answer_all(&server).await;
    let reply = slack.messages().pop().expect("posted, its answer lost");
    assert!(outbound(&pool).await.is_empty(), "not recorded yet");
    posted(
        &server,
        &to,
        "Ev0ECHO",
        json!({
            "type": "message", "subtype": "bot_message", "user": to.bot_user, "bot_id": format!("B{}", to.bot_user),
            "team": TEAM, "channel": "C0GENERAL", "channel_type": "channel", "text": reply.text,
            "ts": reply.ts, "thread_ts": ts, "event_ts": reply.ts,
        }),
        false,
    )
    .await;
    while zupflow_server::channel_inbound::work_once(&server.state).await {}
    assert_eq!(outbound(&pool).await, [(None, 0)], "the echo, as Slack sent it");
    sqlx::query("UPDATE jobs SET run_at = 0 WHERE kind = 'channel_reply'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(zupflow_server::channel_reply::work_once(&server.state).await);
    let run = &channel_runs(&server, &to).await[0];
    assert_eq!(
        outbound(&pool).await,
        [(Some(run_uuid(run)), 1)],
        "one record, filled in"
    );
    assert_eq!(slack.calls_to("chat.postMessage").len(), 1);
    assert!(!zupflow_server::channel_reply::work_once(&server.state).await, "done");
    pool.close().await;
}

/// A sender list matches the author's email as Slack says it, read again a
/// day after it was last read: an address changed since no longer wakes
/// the agent, while the name a run reads stays as first read.
#[tokio::test]
async fn checks_a_sender_list_against_an_email_read_again_after_a_day() {
    let (server, slack) = slack_server().await;
    let to = answering(&server, &slack).await;
    let pool = database(&server).await;
    let alice = |email: &str, name: &str| {
        json!({
            "id": "U0ALICE", "name": "alice", "deleted": false, "is_bot": false,
            "profile": { "display_name": name, "real_name": "Alice Moreau", "email": email },
        })
    };
    slack.add_user(alice("alice@acme.example", "Alice"));
    let channels = format!("/v1/workspaces/{}/agents/{}/channels", to.t.ws, to.agent);
    let (_, listed) = server
        .send(call("alice", Method::GET, channels.clone()).naming(&to.t.org))
        .await;
    let mut settings = listed["data"][0].clone();
    settings["senders"] = json!("listed");
    settings["sender_list"] = json!(["alice@acme.example"]);
    let (status, saved) = server
        .send(
            call("alice", Method::PUT, format!("{channels}/{}", to.connection))
                .naming(&to.t.org)
                .body(settings),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let mention = |seq: u32| {
        let ts = format!("{}.{seq:06}", now_seconds());
        json!({
            "type": "message", "user": "U0ALICE", "team": TEAM, "channel": "C0GENERAL", "channel_type": "channel",
            "text": format!("<@{}> hello", to.bot_user), "ts": ts, "event_ts": ts,
        })
    };
    posted(&server, &to, "Ev0LISTED1", mention(100), false).await;
    answer_all(&server).await;
    assert_eq!(channel_runs(&server, &to).await.len(), 1, "listed");
    assert_eq!(slack.calls_to("users.info").len(), 1);
    // Within the day, the email read is trusted.
    slack.add_user(alice("alice@elsewhere.example", "Ally"));
    posted(&server, &to, "Ev0LISTED2", mention(200), false).await;
    answer_all(&server).await;
    assert_eq!(channel_runs(&server, &to).await.len(), 2);
    assert_eq!(slack.calls_to("users.info").len(), 1);
    // A day on, it's read again: no longer listed.
    let day_ago = now_millis() - 24 * 60 * 60 * 1000 - 1000;
    sqlx::query("UPDATE channel_people SET looked_up_at = ? WHERE external_id = 'U0ALICE'")
        .bind(day_ago)
        .execute(&pool)
        .await
        .unwrap();
    posted(&server, &to, "Ev0LISTED3", mention(300), false).await;
    answer_all(&server).await;
    assert_eq!(slack.calls_to("users.info").len(), 2);
    let runs = channel_runs(&server, &to).await;
    assert_eq!(runs.len(), 2, "the new address isn't listed");
    assert!(
        runs.iter()
            .all(|run| run["input"].as_str().unwrap().contains("[Alice]")),
        "the name stays as first read: {runs:?}"
    );
    // An email due to be read again that Slack can't give now isn't
    // trusted: the message waits for it.
    slack.add_user(alice("alice@acme.example", "Alice"));
    sqlx::query("UPDATE channel_people SET looked_up_at = ? WHERE external_id = 'U0ALICE'")
        .bind(day_ago)
        .execute(&pool)
        .await
        .unwrap();
    slack.fail_next("users.info", Fault::Status(503));
    posted(&server, &to, "Ev0LISTED4", mention(400), false).await;
    answer_all(&server).await;
    assert_eq!(channel_runs(&server, &to).await.len(), 2, "not yet verified");
    sqlx::query("UPDATE jobs SET run_at = 0 WHERE kind = 'channel_inbound' AND state = 'queued'")
        .execute(&pool)
        .await
        .unwrap();
    answer_all(&server).await;
    assert_eq!(channel_runs(&server, &to).await.len(), 3, "listed again, once read");
    assert_eq!(slack.calls_to("users.info").len(), 4);
    pool.close().await;
}
