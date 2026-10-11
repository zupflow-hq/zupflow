//! Tool discovery end to end (decision 0038): connecting an installation
//! queues a job; the worker lists the app's tools through the installation's
//! credential, as the template it connected under places it, and records
//! them pinned as that installation's own.

#![allow(clippy::unwrap_used)]

mod common;
#[path = "common/rotate_on_read.rs"]
mod rotate_on_read;

use axum::http::{Method, StatusCode};
use common::{Server, call};
use serde_json::{Value, json};
use zupflow_core::apps::ToolEffect;
use zupflow_core::ids::{InstallationId, OrganizationId, WorkspaceId};
use zupflow_core::tools::{RegisteredTool, ToolStatus};
use zupflow_server::discovery::{Outcome, discover, work_once};

const MCP: &str = "https://tools.example/mcp";
const PRODUCTION_MCP: &str = "https://tools.example/live/mcp";

fn id(value: &Value) -> &str {
    value["id"].as_str().unwrap()
}

/// An installation's scope, as the API names it.
#[derive(Clone)]
struct Installed {
    org: String,
    ws: String,
    installation: String,
}

impl Installed {
    fn ids(&self) -> (OrganizationId, WorkspaceId, InstallationId) {
        (
            OrganizationId::parse(&self.org).unwrap(),
            WorkspaceId::parse(&self.ws).unwrap(),
            InstallationId::parse(&self.installation).unwrap(),
        )
    }

    async fn tools(&self, server: &Server) -> Vec<RegisteredTool> {
        let (org, ws, installation) = self.ids();
        server
            .state
            .tools
            .list_tools(org, ws, installation, None, 100)
            .await
            .unwrap()
            .items
    }

    async fn discover(&self, server: &Server) -> Outcome {
        let (org, ws, installation) = self.ids();
        discover(&server.state, org, ws, installation).await
    }
}

fn statuses(tools: &[RegisteredTool]) -> Vec<(&str, ToolStatus)> {
    tools.iter().map(|tool| (tool.name.as_str(), tool.status())).collect()
}

/// The token template: the key goes in `X-Api-Key`, after `Key `.
fn token_template() -> Value {
    json!({
        "type": "token",
        "inputs": [{ "key": "api_key", "label": "API key", "secret": true, "help": null, "placeholder": null, "pattern": null }],
        "placement": { "location": "header", "name": "X-Api-Key", "prefix": "Key " },
        "verify_url": null, "instructions": "", "docs_url": null,
    })
}

/// A catalog token app, served at [`MCP`] in Sandbox and [`PRODUCTION_MCP`]
/// in Production.
async fn catalog_token_app(server: &Server, org: &str) -> String {
    let (_, created) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/apps")).body(json!({
                "kind": "mcp", "name": "Tools", "slug": "tools", "description": "", "auth_type": "token",
            })),
        )
        .await;
    let app = id(&created["data"]).to_owned();
    let (status, body) = server
        .send(
            call("alice", Method::PATCH, format!("/v1/organizations/{org}/apps/{app}"))
                .body(json!({ "auth": token_template() })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for (environment, url) in [("sandbox", MCP), ("production", PRODUCTION_MCP)] {
        let (status, _) = server
            .send(
                call(
                    "alice",
                    Method::PUT,
                    format!("/v1/organizations/{org}/apps/{app}/environments/{environment}/configuration"),
                )
                .body(json!({ "mcp_server_url": url })),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    app
}

/// Starts a session as `caller` and submits `key` on the hosted page.
async fn connect_token(server: &Server, caller: &str, org: &str, ws: &str, app: &str, key: &str) -> Installed {
    let (status, session) = server
        .send(
            call(caller, Method::POST, format!("/v1/workspaces/{ws}/connect-sessions"))
                .naming(org)
                .body(json!({ "app_id": app, "intent": "create", "label": "Main", "installation_id": null, "return_url": null })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{session}");
    let reference = session["data"]["url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned();
    let (status, done) = server
        .send(
            call(
                "nobody",
                Method::POST,
                format!("/v1/connect/sessions/{reference}/credentials"),
            )
            .body(json!({ "values": { "api_key": key } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    Installed {
        org: org.to_owned(),
        ws: ws.to_owned(),
        installation: id(&done["data"]["installation"]).to_owned(),
    }
}

async fn api_key(server: &Server, org: &str, environment: &str) -> String {
    let (_, body) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/api-keys"))
                .body(json!({ "name": "Server", "environment": environment })),
        )
        .await;
    body["data"]["secret"].as_str().unwrap().to_owned()
}

async fn database(server: &Server) -> sqlx::SqlitePool {
    sqlx::SqlitePool::connect(&format!("sqlite:{}", server.directory.join("zupflow.sqlite").display()))
        .await
        .unwrap()
}

#[tokio::test]
async fn discovers_an_installations_tools_once_it_connects() {
    let server = Server::new().await;
    server.providers.serve_mcp(
        MCP,
        json!([
            { "name": "search", "description": "Find things", "inputSchema": { "type": "object" }, "annotations": { "readOnlyHint": true } },
            { "name": "delete_all", "inputSchema": { "type": "object" } },
        ]),
    );
    let org = server.organization("alice", "northwind").await;
    let sandbox = id(&server.workspace("alice", &org, "acme-sandbox", "sandbox").await).to_owned();
    let app = catalog_token_app(&server, &org).await;
    let installed = connect_token(&server, "alice", &org, &sandbox, &app, "tk_live_1234").await;
    assert!(
        server.providers.sent().is_empty(),
        "the connect didn't wait for discovery"
    );

    assert!(work_once(&server.state).await, "connecting queued a discovery");
    assert!(!work_once(&server.state).await, "done once");
    let sent = server.providers.sent();
    assert_eq!(sent.len(), 3, "initialize, initialized, tools/list");
    assert!(
        sent.iter()
            .all(|sent| sent.url == MCP
                && sent.credential == Some(("X-Api-Key".to_owned(), "Key tk_live_1234".to_owned()))),
        "the installation's credential, placed as its template says"
    );
    let tools = installed.tools(&server).await;
    assert_eq!(
        tools
            .iter()
            .map(|tool| (tool.name.as_str(), tool.effect, tool.status()))
            .collect::<Vec<_>>(),
        [
            ("delete_all", ToolEffect::Destructive, ToolStatus::Current),
            ("search", ToolEffect::Read, ToolStatus::Current),
        ]
    );

    // A later listing with a changed definition is held, unreviewed; a missing tool is gone.
    server.providers.serve_mcp(
        MCP,
        json!([{ "name": "search", "description": "Find everything", "inputSchema": { "type": "object" }, "annotations": { "readOnlyHint": true } }]),
    );
    assert_eq!(installed.discover(&server).await, Outcome::Done);
    let tools = installed.tools(&server).await;
    assert_eq!(
        statuses(&tools),
        [("delete_all", ToolStatus::Gone), ("search", ToolStatus::Changed)]
    );
    assert!(
        tools[1].pinned.definition.contains("Find things"),
        "the pinned text stays"
    );

    // A server that repeats the credential gets nothing recorded.
    server.providers.serve_mcp(
        MCP,
        json!([{ "name": "search", "description": "Your key: tk_live_1234" }, { "name": "new_tool" }]),
    );
    assert_eq!(installed.discover(&server).await, Outcome::Done);
    let after = installed.tools(&server).await;
    assert_eq!(after, tools, "the whole listing refused");
    assert!(
        after
            .iter()
            .all(|tool| !tool.pinned.definition.contains("tk_live_1234"))
    );
}

/// Each installation lists through its own credential, at its own
/// environment's server, into its own tools; the worker reaches an
/// installation only in its own organization and workspace.
#[tokio::test]
async fn keeps_each_installations_tools_to_itself() {
    let server = Server::new().await;
    server
        .providers
        .serve_mcp(MCP, json!([{ "name": "search", "description": "Sandbox" }]));
    server.providers.serve_mcp(
        PRODUCTION_MCP,
        json!([{ "name": "search", "description": "Production" }]),
    );
    let org = server.organization("alice", "northwind").await;
    let sandbox = id(&server.workspace("alice", &org, "acme-sandbox", "sandbox").await).to_owned();
    let production = id(&server.workspace("alice", &org, "acme", "production").await).to_owned();
    let app = catalog_token_app(&server, &org).await;
    let in_sandbox = connect_token(&server, "alice", &org, &sandbox, &app, "tk_sandbox_1").await;
    let production_key = api_key(&server, &org, "production").await;
    let in_production = connect_token(&server, &production_key, &org, &production, &app, "tk_live_2").await;
    assert!(work_once(&server.state).await);
    assert!(work_once(&server.state).await);
    assert!(!work_once(&server.state).await);

    let sent = server.providers.sent();
    let to = |url: &str| -> Vec<Option<(String, String)>> {
        sent.iter()
            .filter(|sent| sent.url == url)
            .map(|sent| sent.credential.clone())
            .collect()
    };
    assert_eq!(
        to(PRODUCTION_MCP),
        vec![Some(("X-Api-Key".to_owned(), "Key tk_live_2".to_owned())); 3],
        "Production's server, with Production's installation's credential"
    );
    assert_eq!(
        to(MCP),
        vec![Some(("X-Api-Key".to_owned(), "Key tk_sandbox_1".to_owned())); 3]
    );
    for (installed, text) in [(&in_sandbox, "Sandbox"), (&in_production, "Production")] {
        let tools = installed.tools(&server).await;
        assert_eq!(statuses(&tools), [("search", ToolStatus::Current)]);
        assert!(tools[0].pinned.definition.contains(text));
    }

    // Real IDs, wrong scope: another organization's, or a sibling workspace's.
    let other_org = server.organization("riley", "larkspur").await;
    let other_ws = id(&server
        .workspace("riley", &other_org, "larkspur-sandbox", "sandbox")
        .await)
    .to_owned();
    let before = server.providers.sent().len();
    for wrong in [
        Installed {
            org: other_org.clone(),
            ws: in_sandbox.ws.clone(),
            installation: in_sandbox.installation.clone(),
        },
        Installed {
            org: other_org.clone(),
            ws: other_ws.clone(),
            installation: in_sandbox.installation.clone(),
        },
        Installed {
            org: org.clone(),
            ws: production.clone(),
            installation: in_sandbox.installation.clone(),
        },
    ] {
        assert_eq!(wrong.discover(&server).await, Outcome::Done);
    }
    assert_eq!(server.providers.sent().len(), before, "nothing sent for another scope");
}

#[tokio::test]
async fn discovers_a_workspace_apps_tools() {
    let server = Server::new().await;
    server.providers.serve_mcp(MCP, json!([{ "name": "echo" }]));
    let org = server.organization("alice", "northwind").await;
    let sandbox = id(&server.workspace("alice", &org, "acme-sandbox", "sandbox").await).to_owned();
    let (status, created) = server
        .send(
            call("alice", Method::POST, format!("/v1/workspaces/{sandbox}/apps"))
                .naming(&org)
                .body(
                    json!({ "kind": "mcp", "name": "Echo", "slug": "echo", "description": "", "auth_type": "token" }),
                ),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let app = format!("/v1/workspaces/{sandbox}/apps/{}", id(&created["data"]));
    let (status, body) = server
        .send(
            call("alice", Method::PATCH, &app)
                .naming(&org)
                .body(json!({ "auth": token_template() })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = server
        .send(
            call("alice", Method::PUT, format!("{app}/configuration"))
                .naming(&org)
                .body(json!({ "mcp_server_url": MCP })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let installed = connect_token(&server, "alice", &org, &sandbox, id(&created["data"]), "tk_echo").await;
    assert!(work_once(&server.state).await);
    assert_eq!(
        server.providers.sent()[0].credential,
        Some(("X-Api-Key".to_owned(), "Key tk_echo".to_owned()))
    );
    assert_eq!(
        statuses(&installed.tools(&server).await),
        [("echo", ToolStatus::Current)]
    );
}

const TOKEN: &str = "https://accounts.example/token";
const IDENTITY: &str = "https://accounts.example/me";
const TRACKER: &str = "https://tracker.example/mcp";

async fn connect_oauth() -> (Server, Installed) {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let sandbox = id(&server.workspace("alice", &org, "acme-sandbox", "sandbox").await).to_owned();
    let (_, created) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/apps")).body(json!({
                "kind": "mcp", "name": "Tracker", "slug": "tracker", "description": "", "auth_type": "oauth2",
            })),
        )
        .await;
    let app = id(&created["data"]).to_owned();
    server
        .send(
            call("alice", Method::PATCH, format!("/v1/organizations/{org}/apps/{app}")).body(json!({ "auth": {
                "type": "oauth2", "authorize_url": "https://accounts.example/authorize", "token_url": TOKEN,
                "identity_url": IDENTITY, "available_scopes": [{ "name": "read", "description": "" }], "pkce": true,
            }})),
        )
        .await;
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::PUT,
                format!("/v1/organizations/{org}/apps/{app}/environments/sandbox/configuration"),
            )
            .body(json!({
                "mcp_server_url": TRACKER, "oauth_client_id": "client-1",
                "oauth_client_secret": "not-a-real-client-secret-7x2q", "oauth_scopes": ["read"],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, session) = server
        .send(
            call("alice", Method::POST, format!("/v1/workspaces/{sandbox}/connect-sessions"))
                .naming(&org)
                .body(json!({ "app_id": app, "intent": "create", "label": "Main", "installation_id": null, "return_url": null })),
        )
        .await;
    let reference = session["data"]["url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned();
    let (status, body) = server
        .send(call(
            "nobody",
            Method::POST,
            format!("/v1/connect/sessions/{reference}/authorize"),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let consent = url::Url::parse(body["data"]["authorization_url"].as_str().unwrap()).unwrap();
    let state = consent
        .query_pairs()
        .find(|(name, _)| name == "state")
        .unwrap()
        .1
        .into_owned();
    server.providers.script(
        TOKEN,
        200,
        json!({ "access_token": "at-77", "refresh_token": "rt-1", "token_type": "Bearer", "expires_in": 3600, "scope": "read" }),
    );
    server
        .providers
        .script(IDENTITY, 200, json!({ "sub": "u-42", "name": "Jordan" }));
    server.providers.serve_mcp(TRACKER, json!([{ "name": "list_issues" }]));
    let (status, _, _) = server
        .visit(&format!("/connect/oauth/callback?code=c-1&state={state}"))
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, hosted) = server
        .send(call("nobody", Method::GET, format!("/v1/connect/sessions/{reference}")))
        .await;
    let installed = Installed {
        org: org.clone(),
        ws: sandbox.clone(),
        installation: hosted["data"]["installation_id"].as_str().unwrap().to_owned(),
    };

    (server, installed)
}

/// An OAuth app's tools are listed with the installation's access token as a bearer.
#[tokio::test]
async fn discovers_an_oauth_apps_tools_with_its_access_token() {
    let (server, installed) = connect_oauth().await;
    let org = installed.org.clone();
    let sandbox = installed.ws.clone();
    let before = server.providers.sent().len();
    assert!(work_once(&server.state).await, "the callback queued a discovery");
    let sent = server.providers.sent();
    assert_eq!(sent.len(), before + 3);
    assert!(
        sent[before..].iter().all(|sent| sent.url == TRACKER
            && sent.credential == Some(("Authorization".to_owned(), "Bearer at-77".to_owned())))
    );
    assert_eq!(
        statuses(&installed.tools(&server).await),
        [("list_issues", ToolStatus::Current)]
    );

    let org_id = OrganizationId::parse(&org).unwrap();
    let ws_id = WorkspaceId::parse(&sandbox).unwrap();
    let inst_id = InstallationId::parse(&installed.installation).unwrap();
    sqlx::query("UPDATE installations SET expires_at = ? WHERE organization_id = ? AND workspace_id = ? AND id = ?")
        .bind(zupflow_core::time::Timestamp::now().millis() + 10_000)
        .bind(org_id.uuid())
        .bind(ws_id.uuid())
        .bind(inst_id.uuid())
        .execute(&database(&server).await)
        .await
        .unwrap();
    server.providers.script(
        TOKEN,
        200,
        json!({"access_token":"at-rotated", "refresh_token":"rt-rotated", "token_type":"Bearer", "expires_in":3600}),
    );
    let before = server.providers.sent().len();
    assert_eq!(installed.discover(&server).await, Outcome::Done);
    let sent = server.providers.sent();
    assert_eq!(sent[before].url, TOKEN);
    assert!(
        sent[before + 1..]
            .iter()
            .all(|request| request.credential == Some(("Authorization".to_owned(), "Bearer at-rotated".to_owned())))
    );
    let forms = server.providers.bodies();
    let forms: Vec<std::collections::BTreeMap<String, String>> = forms
        .iter()
        .filter(|(url, _)| url == TOKEN)
        .map(|(_, body)| {
            url::form_urlencoded::parse(body.as_ref().unwrap().as_bytes())
                .into_owned()
                .collect()
        })
        .collect();
    assert_eq!(forms.len(), 2);
    assert_eq!(forms[0]["grant_type"], "authorization_code");
    assert!(
        forms.iter().all(|form| !form.contains_key("resource")),
        "configured OAuth refresh preserves the original exchange's absent resource"
    );
    let form = &forms[1];
    assert_eq!(form["grant_type"], "refresh_token");
    assert_eq!(form["refresh_token"], "rt-1");
    let rotated = server
        .state
        .installations
        .find_installation(org_id, ws_id, inst_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rotated.connection.unwrap().credential_version, 2);
    // A stale claimed refresh, even one a process left a day ago, is never
    // sent twice. Discovery reports reconnect and emits the transition.
    sqlx::query("UPDATE installations SET expires_at = 0, refresh_started_at = 1 WHERE organization_id = ? AND workspace_id = ? AND id = ?")
        .bind(org_id.uuid()).bind(ws_id.uuid()).bind(inst_id.uuid()).execute(&database(&server).await).await.unwrap();
    let before = server.providers.sent().len();
    assert_eq!(installed.discover(&server).await, Outcome::Done);
    assert_eq!(server.providers.sent().len(), before);
    assert_eq!(
        server
            .state
            .installations
            .find_installation(org_id, ws_id, inst_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        zupflow_core::installations::InstallationStatus::ReconnectRequired
    );
}

#[tokio::test]
async fn tries_again_only_when_the_server_may_answer_later() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let sandbox = id(&server.workspace("alice", &org, "acme-sandbox", "sandbox").await).to_owned();
    let app = catalog_token_app(&server, &org).await;
    let installed = connect_token(&server, "alice", &org, &sandbox, &app, "tk_live_1234").await;

    // A transient failure keeps the installation connected; a later 401
    // ends discovery and marks its credential as needing a reconnect.
    server.providers.answer(Ok(503));
    assert_eq!(installed.discover(&server).await, Outcome::Retry("server_unavailable"));
    assert_eq!(installed.tools(&server).await, []);

    // A queued job that hits an unavailable server is put back for later.
    assert!(work_once(&server.state).await);
    let job: (String, i64, i64) =
        sqlx::query_as("SELECT state, attempts, run_at FROM jobs WHERE kind = 'discover_tools'")
            .fetch_one(&database(&server).await)
            .await
            .unwrap();
    assert_eq!((job.0.as_str(), job.1), ("queued", 1), "queued again, not completed");
    let wait = job.2 - zupflow_core::time::Timestamp::now().millis();
    assert!((20_000..=30_000).contains(&wait), "due in about 30 seconds: {wait}");
    assert!(!work_once(&server.state).await, "not due again yet");

    // A server refusing the credential ends it.
    sqlx::query("UPDATE jobs SET run_at = 0 WHERE kind = 'discover_tools'")
        .execute(&database(&server).await)
        .await
        .unwrap();
    server.providers.answer(Ok(401));
    assert!(work_once(&server.state).await);
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'discover_tools'")
        .fetch_one(&database(&server).await)
        .await
        .unwrap();
    assert_eq!(left, 0, "completed");
}

struct UnauthorizedOnce {
    inner: std::sync::Arc<dyn zupflow_core::outbound::OutboundHttp>,
    sent: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl zupflow_core::outbound::OutboundHttp for UnauthorizedOnce {
    async fn send(
        &self,
        request: zupflow_core::outbound::OutboundRequest,
        limits: zupflow_core::outbound::Limits,
    ) -> Result<zupflow_core::outbound::OutboundResponse, zupflow_core::outbound::OutboundError> {
        if !self.sent.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Ok(zupflow_core::outbound::OutboundResponse {
                status: 401,
                headers: Vec::new(),
                body: Vec::new(),
            });
        }
        self.inner.send(request, limits).await
    }
}

#[tokio::test]
async fn reads_a_competing_refresh_again_at_each_credential_boundary() {
    use rotate_on_read::{Read, RotateOnRead};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize},
    };
    use zupflow_core::environment::Environment;
    use zupflow_core::installations::NewCredential;
    use zupflow_core::keys::{self, EncryptionContext};

    for (read, at, unauthorized) in [
        (Read::Installation, 1, false),
        (Read::Credential, 2, false),
        (Read::Installation, 2, true),
    ] {
        let (server, installed) = connect_oauth().await;
        let (org, ws, installation) = installed.ids();
        if !unauthorized {
            sqlx::query(
                "UPDATE installations SET expires_at = 0 WHERE organization_id = ? AND workspace_id = ? AND id = ?",
            )
            .bind(org.uuid())
            .bind(ws.uuid())
            .bind(installation.uuid())
            .execute(&database(&server).await)
            .await
            .unwrap();
        }
        let snapshot = server
            .state
            .installations
            .find_installation(org, ws, installation)
            .await
            .unwrap()
            .unwrap();
        let sealed = keys::seal(
            server.state.keys.as_ref(),
            &EncryptionContext::installation_credential(
                org,
                ws,
                Environment::Sandbox,
                snapshot.app.id(),
                installation,
                2,
            ),
            br#"{"access_token":"winner-token","refresh_token":"winner-refresh","token_type":"Bearer"}"#,
        )
        .await
        .unwrap();
        let mut state = server.state.clone();
        state.installations = Arc::new(RotateOnRead {
            inner: state.installations.clone(),
            scope: (org, ws, installation),
            credential: NewCredential { sealed, version: 2 },
            read,
            at,
            reads: AtomicUsize::new(0),
        });
        let before = server.providers.requests_to(TOKEN).len();
        let reach = zupflow_server::mcp_access::reach(&state, org, ws, &snapshot)
            .await
            .unwrap();
        if unauthorized {
            state.outbound = Arc::new(UnauthorizedOnce {
                inner: state.outbound.clone(),
                sent: AtomicBool::new(false),
            });
            zupflow_server::mcp_access::call_tool(
                &state,
                org,
                ws,
                installation,
                reach,
                "list_issues",
                &serde_json::Map::new(),
            )
            .await
            .unwrap();
            assert_eq!(server.providers.tool_calls().len(), 1);
            assert!(
                server
                    .providers
                    .sent()
                    .iter()
                    .filter(|request| request.url == TRACKER)
                    .all(|request| request
                        .credential
                        .as_ref()
                        .is_some_and(|(header, value)| header == "Authorization" && value == "Bearer winner-token"))
            );
        } else {
            assert_eq!(reach.credential().value, "Bearer winner-token");
        }
        assert_eq!(
            server.providers.requests_to(TOKEN).len(),
            before,
            "the loser never exchanges an old refresh token"
        );
        assert_eq!(
            state
                .installations
                .find_installation(org, ws, installation)
                .await
                .unwrap()
                .unwrap()
                .connection
                .unwrap()
                .credential_version,
            2
        );
    }
}
