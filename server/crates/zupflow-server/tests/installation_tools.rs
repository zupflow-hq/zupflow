//! An installation's tools through the API: what discovery recorded, read by
//! any member, and listed again on request (`refresh-tools`), which only
//! queues a discovery. The Console doesn't refresh in Production; a key does.

#![allow(clippy::unwrap_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::http::{Method, StatusCode};
use common::{Server, call, code};
use serde_json::{Value, json};
use zupflow_core::jobs::{Job, JobError, JobId, JobOptions, JobQueue, LeaseToken, Retry};
use zupflow_core::scope::{Name, Scope};
use zupflow_core::storage::{Cache, StorageError};
use zupflow_server::RequestGuard;
use zupflow_server::discovery::work_once;

const MCP: &str = "https://tools.example/mcp";
const PRODUCTION_MCP: &str = "https://tools.example/live/mcp";

fn id(value: &Value) -> &str {
    value["id"].as_str().unwrap()
}

/// A catalog token app, served at [`MCP`] in Sandbox and [`PRODUCTION_MCP`]
/// in Production; the key goes in `X-Api-Key`.
async fn catalog_token_app(server: &Server, org: &str) -> String {
    let (_, created) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/apps")).body(json!({
                "kind": "mcp", "name": "Tools", "key": "tools", "description": "", "auth_type": "token",
            })),
        )
        .await;
    let app = id(&created["data"]).to_owned();
    let (status, body) = server
        .send(
            call("alice", Method::PATCH, format!("/v1/organizations/{org}/apps/{app}")).body(json!({ "auth": {
                "type": "token",
                "inputs": [{ "key": "api_key", "label": "API key", "secret": true, "help": null, "placeholder": null, "pattern": null }],
                "placement": { "location": "header", "name": "X-Api-Key", "prefix": "" },
                "verify_url": null, "instructions": "", "docs_url": null,
            }})),
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

/// Connects an installation through hosted Connect, started by `caller`; its ID.
async fn connect_token(server: &Server, caller: &str, org: &str, ws: &str, app: &str, key: &str) -> String {
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
    id(&done["data"]["installation"]).to_owned()
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

async fn queued_discoveries(server: &Server) -> i64 {
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", server.directory.join("zupflow.sqlite").display()))
        .await
        .unwrap();
    let count = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'discover_tools' AND state = 'queued'")
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    count
}

fn tools_path(ws: &str, installation: &str) -> String {
    format!("/v1/workspaces/{ws}/installations/{installation}/tools")
}

fn refresh_path(ws: &str, installation: &str) -> String {
    format!("/v1/workspaces/{ws}/installations/{installation}/refresh-tools")
}

/// A Sandbox installation of a catalog token app, its tools discovered once.
struct Connected {
    server: Server,
    org: String,
    sandbox: String,
    installation: String,
}

async fn connected() -> Connected {
    let server = Server::new().await;
    server.providers.serve_mcp(
        MCP,
        json!([
            { "name": "search", "title": "Search", "description": "Find things", "inputSchema": { "type": "object", "properties": { "q": { "type": "string" } } }, "annotations": { "readOnlyHint": true } },
            { "name": "archive", "description": "Archive a thing", "inputSchema": { "type": "object" }, "annotations": { "destructiveHint": false } },
            { "name": "drop", "inputSchema": { "type": "object" } },
        ]),
    );
    let org = server.organization("alice", "northwind").await;
    let sandbox = id(&server.workspace("alice", &org, "acme-sandbox", "sandbox").await).to_owned();
    let app = catalog_token_app(&server, &org).await;
    let installation = connect_token(&server, "alice", &org, &sandbox, &app, "tk_sandbox_1234").await;
    assert!(work_once(&server.state).await, "connecting queued a discovery");
    Connected {
        server,
        org,
        sandbox,
        installation,
    }
}

#[tokio::test]
async fn lists_what_discovery_recorded_by_name() {
    let Connected {
        server,
        org,
        sandbox,
        installation,
    } = connected().await;
    let (status, body) = server
        .send(call("alice", Method::GET, tools_path(&sandbox, &installation)).naming(&org))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let tools = body["data"].as_array().unwrap();
    let summary: Vec<(&str, &str, &str)> = tools
        .iter()
        .map(|tool| {
            (
                tool["name"].as_str().unwrap(),
                tool["effect"].as_str().unwrap(),
                tool["status"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("archive", "write", "current"),
            ("drop", "destructive", "current"),
            ("search", "read", "current"),
        ]
    );
    let search = &tools[2];
    assert_eq!(
        search["pinned"],
        json!({
            "title": "Search", "description": "Find things",
            "input_schema": { "type": "object", "properties": { "q": { "type": "string" } } },
            "output_schema": null, "annotations": { "readOnlyHint": true },
        })
    );
    assert_eq!(
        tools[1]["pinned"],
        json!({ "title": null, "description": null, "input_schema": { "type": "object" }, "output_schema": null, "annotations": null })
    );
    assert_eq!(search["latest"], Value::Null);
    assert_eq!(search["changed_at"], Value::Null);
    assert_eq!(search["gone_at"], Value::Null);
    assert!(search["first_seen_at"].as_str().unwrap().ends_with('Z'));

    // A changed definition is shown beside the pinned one; a missing tool is gone.
    server.providers.serve_mcp(
        MCP,
        json!([
            { "name": "search", "title": "Search", "description": "Find everything", "inputSchema": { "type": "object" }, "annotations": { "readOnlyHint": true } },
            { "name": "archive", "description": "Archive a thing", "inputSchema": { "type": "object" }, "annotations": { "destructiveHint": false } },
        ]),
    );
    let (status, body) = server
        .send(call("alice", Method::POST, refresh_path(&sandbox, &installation)).naming(&org))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(id(&body["data"]), installation, "answers the installation");
    assert!(work_once(&server.state).await, "the refresh queued a discovery");
    let (_, body) = server
        .send(call("alice", Method::GET, tools_path(&sandbox, &installation)).naming(&org))
        .await;
    let tools = body["data"].as_array().unwrap();
    let statuses: Vec<&str> = tools.iter().map(|tool| tool["status"].as_str().unwrap()).collect();
    assert_eq!(statuses, ["current", "gone", "changed"]);
    assert!(tools[1]["gone_at"].is_string());
    let search = &tools[2];
    assert_eq!(search["pinned"]["description"], "Find things", "the pinned text stays");
    assert_eq!(
        search["latest"],
        json!({
            "title": "Search", "description": "Find everything", "input_schema": { "type": "object" },
            "output_schema": null, "annotations": { "readOnlyHint": true },
        })
    );
    assert!(search["changed_at"].is_string());
}

#[tokio::test]
async fn keeps_tools_to_their_organization_and_workspace() {
    let Connected {
        server,
        org,
        sandbox,
        installation,
    } = connected().await;
    let sibling = id(&server.workspace("alice", &org, "qa-playground", "sandbox").await).to_owned();
    let other_org = server.organization("riley", "larkspur").await;
    let other_ws = id(&server.workspace("riley", &other_org, "harbor-sandbox", "sandbox").await).to_owned();
    let before = queued_discoveries(&server).await;
    // Real IDs, wrong scope: a sibling workspace's path, another organization's member.
    for (user, named, ws) in [
        ("alice", org.as_str(), sibling.as_str()),
        ("riley", other_org.as_str(), sandbox.as_str()),
        ("riley", other_org.as_str(), other_ws.as_str()),
        ("riley", org.as_str(), sandbox.as_str()),
    ] {
        for (method, path) in [
            (Method::GET, tools_path(ws, &installation)),
            (Method::POST, refresh_path(ws, &installation)),
        ] {
            let (status, body) = server.send(call(user, method, path).naming(named)).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{user} in {ws}: {body}");
            assert_eq!(code(&body), "not_found");
        }
    }
    assert_eq!(queued_discoveries(&server).await, before, "nothing queued");
}

#[tokio::test]
async fn lets_viewers_read_tools_but_not_refresh_them() {
    let Connected {
        server,
        org,
        sandbox,
        installation,
    } = connected().await;
    server.member("alice", &org, "victor", "viewer").await;
    let (status, body) = server
        .send(call("victor", Method::GET, tools_path(&sandbox, &installation)).naming(&org))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"].as_array().unwrap().len(), 3);
    let (status, body) = server
        .send(call("victor", Method::POST, refresh_path(&sandbox, &installation)).naming(&org))
        .await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, "forbidden"));
    assert_eq!(queued_discoveries(&server).await, 0);
}

#[tokio::test]
async fn refreshes_in_production_only_with_an_api_key() {
    let server = Server::new().await;
    server
        .providers
        .serve_mcp(PRODUCTION_MCP, json!([{ "name": "search" }]));
    let org = server.organization("alice", "northwind").await;
    let production = id(&server.workspace("alice", &org, "acme", "production").await).to_owned();
    let app = catalog_token_app(&server, &org).await;
    let key = api_key(&server, &org, "production").await;
    let installation = connect_token(&server, &key, &org, &production, &app, "tk_live_1234").await;
    assert!(work_once(&server.state).await);
    assert!(!work_once(&server.state).await);

    // The Console reads Production's tools but doesn't list them again.
    let (status, body) = server
        .send(call("alice", Method::GET, tools_path(&production, &installation)).naming(&org))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"][0]["name"], "search");
    let (status, body) = server
        .send(call("alice", Method::POST, refresh_path(&production, &installation)).naming(&org))
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "console_read_only_in_production")
    );
    assert_eq!(queued_discoveries(&server).await, 0);

    // The product's key does, and a discovery is queued.
    let (status, body) = server
        .send(call(&key, Method::POST, refresh_path(&production, &installation)))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(id(&body["data"]), installation);
    assert_eq!(queued_discoveries(&server).await, 1);
    let (status, body) = server
        .send(call(&key, Method::GET, tools_path(&production, &installation)))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(work_once(&server.state).await, "the key's refresh queued a discovery");
}

#[tokio::test]
async fn refuses_to_refresh_an_installation_that_isnt_connected() {
    let Connected {
        server,
        org,
        sandbox,
        installation,
    } = connected().await;
    let (status, _) = server
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/workspaces/{sandbox}/installations/{installation}/disconnect"),
            )
            .naming(&org),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = server
        .send(call("alice", Method::POST, refresh_path(&sandbox, &installation)).naming(&org))
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "installation_not_connected")
    );
    assert_eq!(queued_discoveries(&server).await, 0);
    // What was discovered before stays readable.
    let (status, body) = server
        .send(call("alice", Method::GET, tools_path(&sandbox, &installation)).naming(&org))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn pages_tools_by_name_and_shows_everything_hashed() {
    let server = Server::new().await;
    server.providers.serve_mcp(
        MCP,
        json!([
            { "name": "c", "inputSchema": { "type": "object" }, "outputSchema": { "type": "object", "properties": { "ok": { "type": "boolean" } } }, "annotations": { "readOnlyHint": true } },
            { "name": "a", "inputSchema": { "type": "object" } },
            { "name": "B", "inputSchema": { "type": "object" } },
        ]),
    );
    let org = server.organization("alice", "northwind").await;
    let sandbox = id(&server.workspace("alice", &org, "acme-sandbox", "sandbox").await).to_owned();
    let app = catalog_token_app(&server, &org).await;
    let installation = connect_token(&server, "alice", &org, &sandbox, &app, "tk_sandbox_1234").await;
    assert!(work_once(&server.state).await);

    let mut names = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let uri = match &cursor {
            Some(cursor) => format!("{}?limit=2&cursor={cursor}", tools_path(&sandbox, &installation)),
            None => format!("{}?limit=2", tools_path(&sandbox, &installation)),
        };
        let (status, body) = server.send(call("alice", Method::GET, uri).naming(&org)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["meta"]["pagination"]["total"], 3);
        for tool in body["data"].as_array().unwrap() {
            names.push(tool["name"].as_str().unwrap().to_owned());
        }
        match body["meta"]["pagination"]["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
    }
    assert_eq!(names, ["B", "a", "c"], "by name, in byte order, across pages");

    let (_, body) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("{}?cursor=YQ", tools_path(&sandbox, &installation)),
            )
            .naming(&org),
        )
        .await;
    let c = &body["data"][0];
    assert_eq!(c["name"], "c");
    assert_eq!(
        c["pinned"],
        json!({
            "title": null, "description": null, "input_schema": { "type": "object" },
            "output_schema": { "type": "object", "properties": { "ok": { "type": "boolean" } } },
            "annotations": { "readOnlyHint": true },
        })
    );
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("{}?cursor=%25%25", tools_path(&sandbox, &installation)),
            )
            .naming(&org),
        )
        .await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, "validation_failed"));
}

/// Every key reaches only its own organization and environment.
#[tokio::test]
async fn keeps_api_keys_to_their_organization_and_environment() {
    let server = Server::new().await;
    server.providers.serve_mcp(MCP, json!([{ "name": "search" }]));
    server
        .providers
        .serve_mcp(PRODUCTION_MCP, json!([{ "name": "search" }]));
    let org = server.organization("alice", "northwind").await;
    let sandbox = id(&server.workspace("alice", &org, "acme-sandbox", "sandbox").await).to_owned();
    let production = id(&server.workspace("alice", &org, "acme", "production").await).to_owned();
    let app = catalog_token_app(&server, &org).await;
    let sandbox_key = api_key(&server, &org, "sandbox").await;
    let production_key = api_key(&server, &org, "production").await;
    let in_sandbox = connect_token(&server, "alice", &org, &sandbox, &app, "tk_sandbox_1234").await;
    let in_production = connect_token(&server, &production_key, &org, &production, &app, "tk_live_1234").await;
    let other_org = server.organization("riley", "larkspur").await;
    let (_, created) = server
        .send(
            call("riley", Method::POST, format!("/v1/organizations/{other_org}/api-keys"))
                .body(json!({ "name": "Server", "environment": "sandbox" })),
        )
        .await;
    let other_key = created["data"]["secret"].as_str().unwrap().to_owned();
    while work_once(&server.state).await {}
    let before = queued_discoveries(&server).await;

    for (key, ws, installation) in [
        (&sandbox_key, &production, &in_production),
        (&production_key, &sandbox, &in_sandbox),
        (&other_key, &sandbox, &in_sandbox),
    ] {
        for (method, path) in [
            (Method::GET, tools_path(ws, installation)),
            (Method::POST, refresh_path(ws, installation)),
        ] {
            let (status, body) = server.send(call(key, method, path)).await;
            assert_eq!(
                (status, code(&body)),
                (StatusCode::NOT_FOUND, "not_found"),
                "{ws}: {body}"
            );
        }
    }
    assert_eq!(queued_discoveries(&server).await, before, "nothing queued");
    // Each key reaches its own.
    for (key, ws, installation) in [
        (&sandbox_key, &sandbox, &in_sandbox),
        (&production_key, &production, &in_production),
    ] {
        let (status, _) = server.send(call(key, Method::GET, tools_path(ws, installation))).await;
        assert_eq!(status, StatusCode::OK);
    }
}

/// A queue that refuses every job.
struct RefusingQueue;

#[async_trait]
impl JobQueue for RefusingQueue {
    async fn enqueue(&self, _: &Scope, _: &Name, _: Vec<u8>, _: JobOptions) -> Result<JobId, JobError> {
        Err(JobError::Unavailable)
    }
    async fn claim_next(&self, _: &[Name], _: Duration) -> Result<Option<Job>, JobError> {
        Err(JobError::Unavailable)
    }
    async fn complete(&self, _: &Scope, _: JobId, _: &LeaseToken) -> Result<(), JobError> {
        Err(JobError::Unavailable)
    }
    async fn retry(&self, _: &Scope, _: JobId, _: &LeaseToken, _: Duration, _: &Name) -> Result<Retry, JobError> {
        Err(JobError::Unavailable)
    }
    async fn postpone(&self, _: &Scope, _: JobId, _: &LeaseToken, _: Duration) -> Result<(), JobError> {
        Err(JobError::Unavailable)
    }
    async fn bury(&self, _: &Scope, _: JobId, _: &LeaseToken, _: &Name) -> Result<(), JobError> {
        Err(JobError::Unavailable)
    }
    async fn extend(&self, _: &Scope, _: JobId, _: &LeaseToken, _: Duration) -> Result<(), JobError> {
        Err(JobError::Unavailable)
    }
}

#[tokio::test]
async fn answers_503_when_the_refresh_cant_be_queued_and_saves_no_answer() {
    let Connected {
        mut server,
        org,
        sandbox,
        installation,
    } = connected().await;
    let working = server.state.clone();
    let mut refusing = server.state.clone();
    refusing.jobs = Arc::new(RefusingQueue);
    server.app = zupflow_server::app(refusing, RequestGuard::default());
    let (status, body) = server
        .send(
            call("alice", Method::POST, refresh_path(&sandbox, &installation))
                .naming(&org)
                .idempotent("refresh-1"),
        )
        .await;
    assert_eq!((status, code(&body)), (StatusCode::SERVICE_UNAVAILABLE, "unavailable"));
    // The same key once the queue works again runs the refresh, rather than replaying the 503.
    server.app = zupflow_server::app(working, RequestGuard::default());
    let (status, body) = server
        .send(
            call("alice", Method::POST, refresh_path(&sandbox, &installation))
                .naming(&org)
                .idempotent("refresh-1"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(queued_discoveries(&server).await, 1);
}

#[tokio::test]
async fn refuses_a_second_refresh_within_a_minute() {
    let Connected {
        server,
        org,
        sandbox,
        installation,
    } = connected().await;
    let (status, body) = server
        .send(call("alice", Method::POST, refresh_path(&sandbox, &installation)).naming(&org))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = server
        .send(call("alice", Method::POST, refresh_path(&sandbox, &installation)).naming(&org))
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::TOO_MANY_REQUESTS, "refresh_too_soon")
    );
    assert!(body["error"]["message"].as_str().unwrap().contains("minute"));
    assert_eq!(queued_discoveries(&server).await, 1, "only the first was queued");

    // Pacing is per installation: another one in the same workspace refreshes.
    let app = catalog_token_app_named(&server, &org, "other_tools").await;
    let other = connect_token(&server, "alice", &org, &sandbox, &app, "tk_other_1234").await;
    let (status, body) = server
        .send(call("alice", Method::POST, refresh_path(&sandbox, &other)).naming(&org))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// A second catalog token app, so a workspace can hold two installations.
async fn catalog_token_app_named(server: &Server, org: &str, key: &str) -> String {
    let (_, created) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/apps")).body(json!({
                "kind": "mcp", "name": "Other", "key": key, "description": "", "auth_type": "token",
            })),
        )
        .await;
    let app = id(&created["data"]).to_owned();
    let (status, body) = server
        .send(
            call("alice", Method::PATCH, format!("/v1/organizations/{org}/apps/{app}")).body(json!({ "auth": {
                "type": "token",
                "inputs": [{ "key": "api_key", "label": "API key", "secret": true, "help": null, "placeholder": null, "pattern": null }],
                "placement": { "location": "header", "name": "X-Api-Key", "prefix": "" },
                "verify_url": null, "instructions": "", "docs_url": null,
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = server
        .send(
            call(
                "alice",
                Method::PUT,
                format!("/v1/organizations/{org}/apps/{app}/environments/sandbox/configuration"),
            )
            .body(json!({ "mcp_server_url": MCP })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    app
}

/// A cache that can't be reached.
struct UnreachableCache;

#[async_trait]
impl Cache for UnreachableCache {
    async fn get(&self, _: &Scope, _: &Name) -> Result<Option<Vec<u8>>, StorageError> {
        Err(StorageError::Unavailable)
    }
    async fn put(&self, _: &Scope, _: &Name, _: Vec<u8>, _: Duration) -> Result<(), StorageError> {
        Err(StorageError::Unavailable)
    }
    async fn add_if_absent(&self, _: &Scope, _: &Name, _: Vec<u8>, _: Duration) -> Result<bool, StorageError> {
        Err(StorageError::Unavailable)
    }
    async fn delete(&self, _: &Scope, _: &Name) -> Result<(), StorageError> {
        Err(StorageError::Unavailable)
    }
    async fn delete_scope(&self, _: &Scope) -> Result<(), StorageError> {
        Err(StorageError::Unavailable)
    }
}

#[tokio::test]
async fn answers_503_and_queues_nothing_when_pacing_cant_be_checked() {
    let Connected {
        mut server,
        org,
        sandbox,
        installation,
    } = connected().await;
    let mut state = server.state.clone();
    state.cache = Arc::new(UnreachableCache);
    server.app = zupflow_server::app(state, RequestGuard::default());
    let (status, body) = server
        .send(call("alice", Method::POST, refresh_path(&sandbox, &installation)).naming(&org))
        .await;
    assert_eq!((status, code(&body)), (StatusCode::SERVICE_UNAVAILABLE, "unavailable"));
    assert_eq!(queued_discoveries(&server).await, 0);
}
