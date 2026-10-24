//! Channels through the API: a team linked with tokens the provider checked,
//! never shown again; an agent's app made by a job after `202`, at least
//! once (an attempt that ends between the provider's answer and the row
//! that records it leaves nothing behind at the provider); developers change
//! channels, the Console never in Production; and nothing of one workspace
//! or organization reached through another.

#![allow(clippy::unwrap_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::{Method, StatusCode};
use common::{Server, call, code, send_to};
use serde_json::{Value, json};
use zupflow_basic::scripted_channel_provisioner::ScriptedChannelProvisioner;
use zupflow_core::channels::AppStatus;
use zupflow_core::channels::store::AppCreation;
use zupflow_core::ids::{ChannelAppId, OrganizationId, WorkspaceId};
use zupflow_core::keys::{DataKey, EncryptionContext, KeyError, KeyService, WrappedKey};
use zupflow_core::scope::Name;
use zupflow_server::channels::PROVISION_CHANNEL_APP;

const CONFIGURATION_TOKEN: &str = "xoxe.xoxp-1-server-test-7q2k";
const REFRESH_TOKEN: &str = "xoxe-1-server-refresh-9f3d";

fn managers(ws: &str) -> String {
    format!("/v1/workspaces/{ws}/channel-app-managers")
}

fn link<'a>(ws: &str, org: &'a str, who: &'a str, configuration_token: &str) -> common::Call<'a> {
    call(who, Method::POST, managers(ws)).naming(org).body(json!({
        "provider": "slack",
        "method": "configuration_token",
        "configuration_token": configuration_token,
        "refresh_token": REFRESH_TOKEN,
    }))
}

async fn agent(server: &Server, org: &str, ws: &str, name: &str) -> String {
    let (status, body) = server
        .send(
            call("alice", Method::POST, format!("/v1/workspaces/{ws}/agents"))
                .naming(org)
                .body(json!({ "name": name, "description": "", "instructions": "Be brief." })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["data"]["id"].as_str().unwrap().to_owned()
}

/// The app once its job has made it.
async fn made(server: &Server, org: &str, ws: &str, app: &str) -> Value {
    for _ in 0..50 {
        zupflow_server::channels::work_once(&server.state).await;
        let (status, body) = server
            .send(call("alice", Method::GET, format!("/v1/workspaces/{ws}/channel-apps/{app}")).naming(org))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        if body["data"]["status"] != "creating" {
            return body["data"].clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("{app} is still being made");
}

#[tokio::test]
async fn links_a_team_and_makes_an_agents_app_with_a_job() {
    let server = Server::new().await;
    let org = server.organization("alice", "acme").await;
    let ws = server.workspace("alice", &org, "acme-sandbox", "sandbox").await["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let (status, refused) = server.send(link(&ws, &org, "alice", "xoxe.xoxp-1-INVALID")).await;
    assert_eq!(
        (status, code(&refused)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_configuration_token")
    );
    assert!(refused["error"]["fields"]["configuration_token"].is_string());

    let (status, linked) = server.send(link(&ws, &org, "alice", CONFIGURATION_TOKEN)).await;
    assert_eq!(status, StatusCode::CREATED, "{linked}");
    let manager = linked["data"]["id"].as_str().unwrap().to_owned();
    assert_eq!(linked["data"]["status"], "active");
    assert_eq!(linked["data"]["configuration_token"]["last4"], "7q2k");
    let shown = linked.to_string();
    assert!(!shown.contains(CONFIGURATION_TOKEN) && !shown.contains(REFRESH_TOKEN));

    let agent = agent(&server, &org, &ws, "Scribe").await;
    let create = || {
        call("alice", Method::POST, format!("/v1/workspaces/{ws}/channel-apps"))
            .naming(&org)
            .body(json!({ "agent_id": agent, "manager_id": manager }))
    };
    let (status, accepted) = server.send(create()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    assert_eq!(accepted["data"]["status"], "creating");
    let app = accepted["data"]["id"].as_str().unwrap().to_owned();
    let made = made(&server, &org, &ws, &app).await;
    assert_eq!(made["status"], "awaiting_install");
    assert!(made["external_app_id"].as_str().unwrap().starts_with("A0"));
    // Never the app's secrets or where its events go.
    assert!(made.get("inbound_ref").is_none() && made.get("signing_secret").is_none());

    let (status, again) = server.send(create()).await;
    assert_eq!((status, code(&again)), (StatusCode::CONFLICT, "channel_app_exists"));
    let (_, read) = server
        .send(call("alice", Method::GET, format!("{}/{manager}", managers(&ws))).naming(&org))
        .await;
    assert_eq!(read["data"]["app_count"], 1);
    let (status, refused) = server
        .send(call("alice", Method::DELETE, format!("{}/{manager}", managers(&ws))).naming(&org))
        .await;
    assert_eq!((status, code(&refused)), (StatusCode::CONFLICT, "in_use"));
    let (status, _) = server
        .send(
            call(
                "alice",
                Method::DELETE,
                format!("/v1/workspaces/{ws}/channel-apps/{app}"),
            )
            .naming(&org),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Its removal at the provider runs on the worker; the team is in use until then.
    let unlink = || call("alice", Method::DELETE, format!("{}/{manager}", managers(&ws))).naming(&org);
    let mut unlinked = server.send(unlink()).await;
    for _ in 0..50 {
        if unlinked.0 != StatusCode::CONFLICT {
            break;
        }
        assert_eq!(code(&unlinked.1), "in_use");
        zupflow_server::channels::work_once(&server.state).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        unlinked = server.send(unlink()).await;
    }
    assert_eq!(unlinked.0, StatusCode::NO_CONTENT, "{}", unlinked.1);
}

#[tokio::test]
async fn keeps_viewers_out_everywhere_and_the_console_out_of_production() {
    let server = Server::new().await;
    let org = server.organization("alice", "acme").await;
    server.member("alice", &org, "vera", "viewer").await;
    let sandbox = server.workspace("alice", &org, "acme-sandbox", "sandbox").await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let production = server.workspace("alice", &org, "acme", "production").await["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Refused before the body is read: a viewer learns nothing about what it sent.
    let (status, body) = server
        .send(
            call("vera", Method::POST, managers(&sandbox))
                .naming(&org)
                .body(json!({ "nonsense": true })),
        )
        .await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, "forbidden"));
    let (status, body) = server.send(link(&production, &org, "alice", CONFIGURATION_TOKEN)).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "console_read_only_in_production")
    );
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/workspaces/{production}/connect-sessions"),
            )
            .naming(&org)
            .body(json!({ "target": { "kind": "channel_app_manager", "provider": "slack" }, "return_url": null })),
        )
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "console_read_only_in_production")
    );
    let (_, listed) = server
        .send(call("alice", Method::GET, managers(&production)).naming(&org))
        .await;
    assert_eq!(listed["data"], json!([]));

    // Organization settings: an admin's, from the Console.
    let (status, body) = server
        .send(
            call("vera", Method::PUT, format!("/v1/organizations/{org}/channel-settings")).body(json!({
                "breaker_runs": 5, "breaker_window_minutes": 10, "breaker_pause_minutes": 10, "message_retention_days": 30,
            })),
        )
        .await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, "forbidden"));
    let (status, saved) = server
        .send(
            call("alice", Method::PUT, format!("/v1/organizations/{org}/channel-settings")).body(json!({
                "breaker_runs": 5, "breaker_window_minutes": 10, "breaker_pause_minutes": 10, "message_retention_days": 30,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["data"]["breaker_runs"], 5);
}

#[tokio::test]
async fn reaches_nothing_of_another_workspace_or_organization() {
    let server = Server::new().await;
    let org = server.organization("alice", "acme").await;
    let ws = server.workspace("alice", &org, "acme-sandbox", "sandbox").await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let sibling = server.workspace("alice", &org, "qa", "sandbox").await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let other = server.organization("olga", "harbor").await;
    let (_, linked) = server.send(link(&ws, &org, "alice", CONFIGURATION_TOKEN)).await;
    let manager = linked["data"]["id"].as_str().unwrap().to_owned();

    // Another workspace of the same organization: not found, nor changed.
    let (status, _) = server
        .send(call("alice", Method::GET, format!("{}/{manager}", managers(&sibling))).naming(&org))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = server
        .send(
            call("alice", Method::PATCH, format!("{}/{manager}", managers(&sibling)))
                .naming(&org)
                .body(json!({ "configuration_token": "xoxe.xoxp-1-moved-0000", "refresh_token": REFRESH_TOKEN })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, listed) = server
        .send(call("alice", Method::GET, managers(&sibling)).naming(&org))
        .await;
    assert_eq!(listed["data"], json!([]));
    // Another organization's person, naming their own organization or this one.
    for named in [&other, &org] {
        let (status, _) = server
            .send(call("olga", Method::GET, format!("{}/{manager}", managers(&ws))).naming(named))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    let (_, read) = server
        .send(call("alice", Method::GET, format!("{}/{manager}", managers(&ws))).naming(&org))
        .await;
    assert_eq!(read["data"]["configuration_token"]["last4"], "7q2k");
}

/// Keys that refuse to seal as many times as a test arms them.
struct FlakyKeys {
    inner: Arc<dyn KeyService>,
    refusals: AtomicU32,
}

#[async_trait]
impl KeyService for FlakyKeys {
    async fn wrap(&self, context: &EncryptionContext, key: &DataKey) -> Result<WrappedKey, KeyError> {
        if self
            .refusals
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(KeyError::Unavailable);
        }
        self.inner.wrap(context, key).await
    }

    async fn unwrap(&self, context: &EncryptionContext, wrapped: &WrappedKey) -> Result<DataKey, KeyError> {
        self.inner.unwrap(context, wrapped).await
    }
}

/// A server whose provider and keys the test drives, with a team linked and
/// an agent to add to it. Its jobs are made due at once, rather than after
/// their backoff.
struct Driven {
    _server: Server,
    state: zupflow_server::AppState,
    router: axum::Router,
    provider: ScriptedChannelProvisioner,
    keys: Arc<FlakyKeys>,
    pool: sqlx::SqlitePool,
    org: String,
    ws: String,
    manager: String,
    agent: String,
}

impl Driven {
    async fn new() -> Self {
        let server = Server::new().await;
        let provider = ScriptedChannelProvisioner::default();
        let keys = Arc::new(FlakyKeys {
            inner: server.state.keys.clone(),
            refusals: AtomicU32::new(0),
        });
        let mut state = server.state.clone();
        state.channel_provisioner = Arc::new(provider.clone());
        state.keys = keys.clone();
        let router = zupflow_server::app(state.clone(), zupflow_server::RequestGuard::default());
        let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", server.directory.join("zupflow.sqlite").display()))
            .await
            .unwrap();
        let org = server.organization("alice", "acme").await;
        let ws = server.workspace("alice", &org, "acme-sandbox", "sandbox").await["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let (status, linked) = send_to(&router, link(&ws, &org, "alice", CONFIGURATION_TOKEN)).await;
        assert_eq!(status, StatusCode::CREATED, "{linked}");
        let manager = linked["data"]["id"].as_str().unwrap().to_owned();
        let agent = agent(&server, &org, &ws, "Scribe").await;
        Self {
            _server: server,
            state,
            router,
            provider,
            keys,
            pool,
            org,
            ws,
            manager,
            agent,
        }
    }

    async fn create_app(&self) -> String {
        let (status, accepted) = send_to(
            &self.router,
            call(
                "alice",
                Method::POST,
                format!("/v1/workspaces/{}/channel-apps", self.ws),
            )
            .naming(&self.org)
            .body(json!({ "agent_id": self.agent, "manager_id": self.manager })),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
        accepted["data"]["id"].as_str().unwrap().to_owned()
    }

    async fn creation(&self, app: &str) -> AppCreation {
        self.state
            .channels
            .app_creation(
                OrganizationId::parse(&self.org).unwrap(),
                WorkspaceId::parse(&self.ws).unwrap(),
                ChannelAppId::parse(app).unwrap(),
            )
            .await
            .unwrap()
            .unwrap()
    }

    /// Each provisioning job's state and attempts so far.
    async fn jobs(&self) -> Vec<(String, i64)> {
        sqlx::query_as("SELECT state, attempts FROM jobs WHERE kind = ? ORDER BY created_at")
            .bind(PROVISION_CHANNEL_APP)
            .fetch_all(&self.pool)
            .await
            .unwrap()
    }

    async fn due_now(&self) {
        sqlx::query("UPDATE jobs SET run_at = 0 WHERE state = 'queued'")
            .execute(&self.pool)
            .await
            .unwrap();
    }

    /// Works the app's jobs, retries due at once, until it's settled: no
    /// longer being made, nothing in flight, and no job left for it.
    async fn settle(&self, app: &str) -> AppCreation {
        for _ in 0..100 {
            self.due_now().await;
            zupflow_server::channels::work_once(&self.state).await;
            let creation = self.creation(app).await;
            let live = self.jobs().await.iter().filter(|(state, _)| state != "dead").count();
            if creation.app.status != AppStatus::Creating && !creation.in_flight && live == 0 {
                return creation;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{app} never settled: {:?}", self.creation(app).await);
    }
}

#[tokio::test]
async fn makes_an_app_once_when_the_providers_answer_is_lost() {
    let d = Driven::new().await;
    d.provider.lose_next_answer();
    let app = d.create_app().await;
    let creation = d.settle(&app).await;
    let made = d.provider.apps();
    assert_eq!(made.len(), 2, "{made:?}");
    assert!(made[0].deleted, "the app whose answer was lost is found and deleted");
    assert!(!made[1].deleted);
    assert_eq!(creation.app.status, AppStatus::AwaitingInstall);
    assert_eq!(
        creation.app.external_app_id.as_deref(),
        Some(made[1].external_app_id.as_str())
    );
}

#[tokio::test]
async fn makes_an_app_once_when_its_secrets_cant_be_kept() {
    let d = Driven::new().await;
    // Made and its ID recorded; then sealing its secrets fails, and so does
    // deleting it at once: the next attempt deletes it before making another.
    d.keys.refusals.store(1, Ordering::SeqCst);
    d.provider.fail_next_delete();
    let app = d.create_app().await;
    let creation = d.settle(&app).await;
    let made = d.provider.apps();
    assert_eq!(made.len(), 2, "{made:?}");
    assert!(made[0].deleted, "the app whose secrets were lost is deleted");
    assert!(!made[1].deleted);
    assert_eq!(creation.app.status, AppStatus::AwaitingInstall);
    assert_eq!(
        creation.app.external_app_id.as_deref(),
        Some(made[1].external_app_id.as_str())
    );
    let secrets = d
        .state
        .channels
        .app_secrets(
            OrganizationId::parse(&d.org).unwrap(),
            WorkspaceId::parse(&d.ws).unwrap(),
            ChannelAppId::parse(&app).unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        secrets.secrets.map(|sealed| sealed.version),
        Some(1),
        "one sealed version"
    );
}

#[tokio::test]
async fn deletes_at_the_provider_an_app_removed_while_it_was_made() {
    let d = Driven::new().await;
    let (router, org, ws) = (d.router.clone(), d.org.clone(), d.ws.clone());
    d.provider.during_next_create(Box::new(move || {
        Box::pin(async move {
            let apps = format!("/v1/workspaces/{ws}/channel-apps");
            let (_, listed) = send_to(&router, call("alice", Method::GET, apps.clone()).naming(&org)).await;
            let app = listed["data"][0]["id"].as_str().unwrap().to_owned();
            let (status, _) = send_to(
                &router,
                call("alice", Method::DELETE, format!("{apps}/{app}")).naming(&org),
            )
            .await;
            assert_eq!(status, StatusCode::NO_CONTENT);
        })
    }));
    let app = d.create_app().await;
    let creation = d.settle(&app).await;
    assert_eq!((creation.app.status, creation.in_flight), (AppStatus::Deleted, false));
    let made = d.provider.apps();
    assert_eq!(made.len(), 1, "{made:?}");
    assert!(
        made[0].deleted,
        "what was made for a removed app is deleted at the provider"
    );
}

#[tokio::test]
async fn fails_an_app_whose_job_is_buried_and_cleans_up_after_it() {
    let d = Driven::new().await;
    // Its first attempt makes it, but the answer is lost; then the job is
    // buried, as when its last attempt's lease runs out.
    d.provider.lose_next_answer();
    let app = d.create_app().await;
    for _ in 0..100 {
        if d.jobs().await == [("queued".to_owned(), 1)] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(d.jobs().await, [("queued".to_owned(), 1)]);
    d.due_now().await;
    let kind = Name::new(PROVISION_CHANNEL_APP).unwrap();
    let job = d
        .state
        .jobs
        .claim_next(&[kind], Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    d.state
        .jobs
        .bury(&job.scope, job.id, &job.lease, &Name::new("lease_expired").unwrap())
        .await
        .unwrap();
    let stranded = d.creation(&app).await;
    assert_eq!((stranded.app.status, stranded.in_flight), (AppStatus::Creating, true));

    assert_eq!(zupflow_server::channels::reap_once(&d.state).await, 1);
    let (status, failed) = send_to(
        &d.router,
        call(
            "alice",
            Method::GET,
            format!("/v1/workspaces/{}/channel-apps/{app}", d.ws),
        )
        .naming(&d.org),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(failed["data"]["status"], "failed", "{failed}");
    assert_eq!(failed["data"]["last_error"], "provisioning_abandoned");
    // The job the sweep queued finds what was made and deletes it.
    let settled = d.settle(&app).await;
    assert_eq!((settled.app.status, settled.in_flight), (AppStatus::Failed, false));
    let made = d.provider.apps();
    assert_eq!(made.len(), 1, "{made:?}");
    assert!(made[0].deleted);
    assert_eq!(
        zupflow_server::channels::reap_once(&d.state).await,
        0,
        "settled for good"
    );

    // Failed isn't live: the agent can be given another app.
    let again = d.create_app().await;
    assert_eq!(d.settle(&again).await.app.status, AppStatus::AwaitingInstall);
}
