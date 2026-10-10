//! Agents and grants through the API (decision 0040): any member reads them,
//! developers and stricter change them, an API key acts in its own
//! organization and environment; a grant names only tools its installation's
//! registry holds and doesn't hold as gone; models come from the catalog.

#![allow(clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use common::{Server, call, code};
use serde_json::{Value, json};
use zupflow_core::apps::ToolEffect;
use zupflow_core::ids::{InstallationId, OrganizationId, WorkspaceId};
use zupflow_core::time::Timestamp;
use zupflow_core::tools::{ListedTool, ToolRevision};
use zupflow_server::RequestGuard;
use zupflow_server::discovery::work_once;

const MCP: &str = "https://tools.example/mcp";

fn id(value: &Value) -> &str {
    value["id"].as_str().unwrap()
}

fn agents_path(ws: &str) -> String {
    format!("/v1/workspaces/{ws}/agents")
}

fn grants_path(ws: &str, agent: &str) -> String {
    format!("/v1/workspaces/{ws}/agents/{agent}/grants")
}

/// A catalog token app served at [`MCP`] in both environments.
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
            call("alice", Method::PATCH, format!("/v1/organizations/{org}/apps/{app}")).body(json!({ "auth": {
                "type": "token",
                "inputs": [{ "key": "api_key", "label": "API key", "secret": true, "help": null, "placeholder": null, "pattern": null }],
                "placement": { "location": "header", "name": "X-Api-Key", "prefix": "" },
                "verify_url": null, "instructions": "", "docs_url": null,
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for environment in ["sandbox", "production"] {
        let (status, _) = server
            .send(
                call(
                    "alice",
                    Method::PUT,
                    format!("/v1/organizations/{org}/apps/{app}/environments/{environment}/configuration"),
                )
                .body(json!({ "mcp_server_url": MCP })),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    app
}

/// Connects an installation through hosted Connect, started by `caller`; its ID.
async fn connect_token(server: &Server, caller: &str, org: &str, ws: &str, app: &str) -> String {
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
            .body(json!({ "values": { "api_key": "tk_1234" } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    while work_once(&server.state).await {}
    id(&done["data"]["installation"]).to_owned()
}

async fn api_key(server: &Server, user: &str, org: &str, environment: &str) -> String {
    let (_, body) = server
        .send(
            call(user, Method::POST, format!("/v1/organizations/{org}/api-keys"))
                .body(json!({ "name": "Server", "environment": environment })),
        )
        .await;
    body["data"]["secret"].as_str().unwrap().to_owned()
}

/// Records a listing straight into the registry, as discovery would.
async fn list_tools(server: &Server, org: &str, ws: &str, installation: &str, names: &[(&str, u8)]) {
    let listed: Vec<ListedTool> = names
        .iter()
        .map(|(name, version)| ListedTool {
            name: (*name).to_owned(),
            revision: ToolRevision {
                definition: format!(r#"{{"description":"v{version}","name":"{name}"}}"#),
                hash: [*version; 32],
            },
            effect: ToolEffect::Read,
        })
        .collect();
    server
        .state
        .tools
        .record_listing(
            OrganizationId::parse(org).unwrap(),
            WorkspaceId::parse(ws).unwrap(),
            InstallationId::parse(installation).unwrap(),
            &listed,
            Timestamp::now(),
        )
        .await
        .unwrap();
}

/// A Sandbox workspace with an installation whose registry holds `search`
/// as listed, `archive` changed since and `drop` gone.
struct Setup {
    server: Server,
    org: String,
    sandbox: String,
    installation: String,
}

async fn setup() -> Setup {
    let server = Server::new().await;
    server.providers.serve_mcp(
        MCP,
        json!([
            { "name": "search", "inputSchema": { "type": "object" }, "annotations": { "readOnlyHint": true } },
            { "name": "archive", "inputSchema": { "type": "object" } },
            { "name": "drop", "inputSchema": { "type": "object" } },
        ]),
    );
    let org = server.organization("alice", "northwind").await;
    let sandbox = id(&server.workspace("alice", &org, "acme-sandbox", "sandbox").await).to_owned();
    let app = catalog_token_app(&server, &org).await;
    let installation = connect_token(&server, "alice", &org, &sandbox, &app).await;
    // Listed again: `archive` differently, `drop` not at all.
    server.providers.serve_mcp(
        MCP,
        json!([
            { "name": "search", "inputSchema": { "type": "object" }, "annotations": { "readOnlyHint": true } },
            { "name": "archive", "description": "Archive, then notify everyone", "inputSchema": { "type": "object" } },
        ]),
    );
    zupflow_server::discovery::enqueue(
        &server.state,
        OrganizationId::parse(&org).unwrap(),
        WorkspaceId::parse(&sandbox).unwrap(),
        InstallationId::parse(&installation).unwrap(),
    )
    .await
    .unwrap();
    while work_once(&server.state).await {}
    let (_, tools) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("/v1/workspaces/{sandbox}/installations/{installation}/tools"),
            )
            .naming(&org),
        )
        .await;
    let statuses: Vec<(&str, &str)> = tools["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| (tool["name"].as_str().unwrap(), tool["status"].as_str().unwrap()))
        .collect();
    assert_eq!(
        statuses,
        [("archive", "changed"), ("drop", "gone"), ("search", "current")]
    );
    Setup {
        server,
        org,
        sandbox,
        installation,
    }
}

async fn create_agent(server: &Server, user: &str, org: &str, ws: &str, name: &str) -> Value {
    let (status, body) = server
        .send(
            call(user, Method::POST, agents_path(ws))
                .naming(org)
                .body(json!({ "name": name, "description": "", "model": "model-a", "instructions": "" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["data"].clone()
}

#[tokio::test]
async fn lists_the_configured_models_to_any_caller() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let key = api_key(&server, "alice", &org, "sandbox").await;
    for who in ["alice", "bob", key.as_str()] {
        let (status, body) = server.send(call(who, Method::GET, "/v1/models")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["data"],
            json!([
                { "id": "model-a", "name": "Model A", "provider": "Example", "description": "The default." },
                { "id": "model-b", "name": "Model B", "provider": "Example", "description": "" },
            ])
        );
    }
}

#[tokio::test]
async fn creates_reads_changes_and_deletes_agents() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    // Production too: agents are configuration, which the Console may change there.
    let ws = id(&server.workspace("alice", &org, "acme", "production").await).to_owned();
    let (status, body) = server
        .send(call("alice", Method::POST, agents_path(&ws)).naming(&org).body(json!({
            "name": "  Publisher  ", "description": " Drafts posts. ", "model": "model-b", "instructions": "  Be brief.\n",
        })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let agent = body["data"].clone();
    let agent_id = id(&agent).to_owned();
    assert!(agent_id.starts_with("agt_"));
    assert_eq!(
        (
            &agent["workspace_id"],
            &agent["name"],
            &agent["description"],
            &agent["model"],
            &agent["instructions"],
        ),
        (
            &json!(ws),
            &json!("Publisher"),
            &json!("Drafts posts."),
            &json!("model-b"),
            &json!("  Be brief.\n"),
        ),
        "names and descriptions trimmed, instructions kept as written"
    );
    assert_eq!(
        (
            &agent["status"],
            &agent["grant_count"],
            &agent["runs_last_24h"],
            &agent["last_run_at"]
        ),
        (&json!("active"), &json!(0), &json!(0), &Value::Null)
    );
    let path = format!("{}/{agent_id}", agents_path(&ws));
    let (status, read) = server.send(call("alice", Method::GET, &path).naming(&org)).await;
    assert_eq!((status, &read["data"]), (StatusCode::OK, &agent));

    let (status, paused) = server
        .send(
            call("alice", Method::PATCH, &path)
                .naming(&org)
                .body(json!({ "status": "paused" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{paused}");
    assert_eq!(
        (&paused["data"]["status"], &paused["data"]["name"]),
        (&json!("paused"), &json!("Publisher"))
    );
    create_agent(&server, "alice", &org, &ws, "Archivist").await;
    let (_, list) = server
        .send(call("alice", Method::GET, agents_path(&ws)).naming(&org))
        .await;
    let names: Vec<&str> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Archivist", "Publisher"], "by name");

    let (status, _) = server.send(call("alice", Method::DELETE, &path).naming(&org)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    for method in [Method::GET, Method::DELETE] {
        let (status, body) = server.send(call("alice", method, &path).naming(&org)).await;
        assert_eq!((status, code(&body)), (StatusCode::NOT_FOUND, "not_found"));
    }
    let (_, workspace) = server
        .send(call("alice", Method::GET, format!("/v1/workspaces/{ws}")).naming(&org))
        .await;
    assert_eq!(
        workspace["data"]["stats"]["agents"], 1,
        "counted in the workspace's stats"
    );
}

#[tokio::test]
async fn checks_every_field_and_the_model() {
    let mut server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let ws = id(&server.workspace("alice", &org, "acme-sandbox", "sandbox").await).to_owned();
    let fields_of = |body: &Value| -> Vec<String> {
        body["error"]["fields"]
            .as_object()
            .map(|fields| fields.keys().cloned().collect())
            .unwrap_or_default()
    };
    for (body, invalid) in [
        (json!({}), vec!["description", "instructions", "model", "name"]),
        (
            json!({ "name": "   ", "description": "", "model": "model-a", "instructions": "" }),
            vec!["name"],
        ),
        (
            json!({ "name": "x".repeat(61), "description": "d".repeat(281), "model": "model-a", "instructions": "i".repeat(20_001) }),
            vec!["description", "instructions", "name"],
        ),
        (
            json!({ "name": "A", "description": "", "model": "model-c", "instructions": "" }),
            vec!["model"],
        ),
        (
            json!({ "name": "A", "description": "", "model": " model-a", "instructions": "" }),
            vec!["model"],
        ),
        (
            json!({ "name": 1, "description": null, "model": "model-a", "instructions": "" }),
            vec!["description", "name"],
        ),
    ] {
        let (status, answer) = server
            .send(
                call("alice", Method::POST, agents_path(&ws))
                    .naming(&org)
                    .body(body.clone()),
            )
            .await;
        assert_eq!(
            (status, code(&answer), fields_of(&answer)),
            (
                StatusCode::BAD_REQUEST,
                "validation_failed",
                invalid.iter().map(|f| (*f).to_owned()).collect::<Vec<_>>()
            ),
            "{body}"
        );
    }
    let agent = create_agent(&server, "alice", &org, &ws, "Valid").await;
    let path = format!("{}/{}", agents_path(&ws), id(&agent));
    for (body, invalid) in [
        (json!({ "model": "model-c" }), "model"),
        (json!({ "status": "archived" }), "status"),
        (json!({ "name": "" }), "name"),
    ] {
        let (status, answer) = server
            .send(call("alice", Method::PATCH, &path).naming(&org).body(body.clone()))
            .await;
        assert_eq!(
            (status, fields_of(&answer)),
            (StatusCode::BAD_REQUEST, vec![invalid.to_owned()]),
            "{body}"
        );
    }

    // With no models configured, no agent can be created, with the same refusal.
    let mut state = server.state.clone();
    state.models = Arc::new(zupflow_basic::configured_models::ConfiguredModels::none());
    server.app = zupflow_server::app(state, RequestGuard::default());
    let (status, models) = server.send(call("alice", Method::GET, "/v1/models")).await;
    assert_eq!((status, &models["data"]), (StatusCode::OK, &json!([])));
    let (status, answer) = server
        .send(call("alice", Method::POST, agents_path(&ws)).naming(&org).body(json!({
            "name": "A", "description": "", "model": "model-a", "instructions": "",
        })))
        .await;
    assert_eq!(
        (status, code(&answer), fields_of(&answer)),
        (StatusCode::BAD_REQUEST, "validation_failed", vec!["model".to_owned()])
    );
}

#[tokio::test]
async fn grants_only_what_the_installations_registry_holds() {
    let Setup {
        server,
        org,
        sandbox,
        installation,
    } = setup().await;
    let agent = create_agent(&server, "alice", &org, &sandbox, "Granted").await;
    let path = grants_path(&sandbox, id(&agent));
    let grant = |tools: Value, approval: Value| {
        call("alice", Method::POST, path.clone())
            .naming(&org)
            .body(json!({ "installation_id": installation, "tools": tools, "approval_required": approval }))
    };
    for (tools, approval, status, expected, field) in [
        (json!(["missing"]), json!([]), 422, "unknown_tool", "tools"),
        (json!(["search", "drop"]), json!([]), 422, "tool_gone", "tools"),
        (json!(["drop", "missing"]), json!([]), 422, "tool_gone", "tools"),
        (
            json!(["search"]),
            json!(["archive"]),
            422,
            "approval_outside_grant",
            "approval_required",
        ),
        (json!([]), json!([]), 400, "validation_failed", "tools"),
        (json!("search"), json!([]), 400, "validation_failed", "tools"),
        (json!(["search", 1]), json!([]), 400, "validation_failed", "tools"),
        (
            json!(["search"]),
            Value::Null,
            400,
            "validation_failed",
            "approval_required",
        ),
    ] {
        let (answered, body) = server.send(grant(tools.clone(), approval.clone())).await;
        assert_eq!(
            (answered.as_u16(), code(&body)),
            (status, expected),
            "{tools} {approval}: {body}"
        );
        assert!(body["error"]["fields"][field].is_string(), "{body}");
    }
    // A malformed or unknown installation is the same 404.
    for other in ["inst_nope", "x"] {
        let (status, body) = server
            .send(call("alice", Method::POST, path.clone()).naming(&org).body(json!({
                "installation_id": other, "tools": ["search"], "approval_required": [],
            })))
            .await;
        assert_eq!((status, code(&body)), (StatusCode::NOT_FOUND, "not_found"));
    }

    // A changed tool can be granted.
    let (status, created) = server
        .send(grant(json!(["search", "archive"]), json!(["archive"])))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let created = created["data"].clone();
    assert!(id(&created).starts_with("grt_"));
    assert_eq!(
        (
            &created["agent_id"],
            &created["installation_id"],
            &created["tools"],
            &created["approval_required"]
        ),
        (
            &agent["id"],
            &json!(installation),
            &json!(["search", "archive"]),
            &json!(["archive"])
        )
    );
    let (status, body) = server.send(grant(json!(["search"]), json!([]))).await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "grant_exists"));

    let grant_path = format!("{path}/{}", id(&created));
    for (tools, expected) in [
        (json!(["search", "drop"]), "tool_gone"),
        (json!(["missing"]), "unknown_tool"),
    ] {
        let (status, body) = server
            .send(
                call("alice", Method::PATCH, &grant_path)
                    .naming(&org)
                    .body(json!({ "tools": tools, "approval_required": [] })),
            )
            .await;
        assert_eq!((status, code(&body)), (StatusCode::UNPROCESSABLE_ENTITY, expected));
    }
    let (status, body) = server
        .send(
            call("alice", Method::PATCH, &grant_path)
                .naming(&org)
                .body(json!({ "tools": [], "approval_required": [] })),
        )
        .await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, "validation_failed"));
    let (status, updated) = server
        .send(
            call("alice", Method::PATCH, &grant_path)
                .naming(&org)
                .body(json!({ "tools": ["archive"], "approval_required": [] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["data"]["tools"], json!(["archive"]));

    // Counted on the agent and on the installation.
    let (_, read) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("{}/{}", agents_path(&sandbox), id(&agent)),
            )
            .naming(&org),
        )
        .await;
    assert_eq!(read["data"]["grant_count"], 1);
    let (_, inst) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("/v1/workspaces/{sandbox}/installations/{installation}"),
            )
            .naming(&org),
        )
        .await;
    assert_eq!(inst["data"]["grant_count"], 1);

    // A tool that goes later stays in the grant; granting it again is refused.
    list_tools(&server, &org, &sandbox, &installation, &[("search", 9)]).await;
    let (_, grants) = server.send(call("alice", Method::GET, &path).naming(&org)).await;
    assert_eq!(grants["data"][0]["tools"], json!(["archive"]));
    let (status, body) = server
        .send(
            call("alice", Method::PATCH, &grant_path)
                .naming(&org)
                .body(json!({ "tools": ["archive"], "approval_required": [] })),
        )
        .await;
    assert_eq!((status, code(&body)), (StatusCode::UNPROCESSABLE_ENTITY, "tool_gone"));

    let (status, _) = server
        .send(call("alice", Method::DELETE, &grant_path).naming(&org))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = server
        .send(call("alice", Method::DELETE, &grant_path).naming(&org))
        .await;
    assert_eq!((status, code(&body)), (StatusCode::NOT_FOUND, "not_found"));
}

#[tokio::test]
async fn deletes_grants_with_their_agent_or_installation() {
    let Setup {
        server,
        org,
        sandbox,
        installation,
    } = setup().await;
    let body = json!({ "installation_id": installation, "tools": ["search"], "approval_required": [] });
    let mut agents = Vec::new();
    for name in ["Kept", "Deleted"] {
        let agent = create_agent(&server, "alice", &org, &sandbox, name).await;
        let (status, _) = server
            .send(
                call("alice", Method::POST, grants_path(&sandbox, id(&agent)))
                    .naming(&org)
                    .body(body.clone()),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED);
        agents.push(id(&agent).to_owned());
    }
    let installation_path = format!("/v1/workspaces/{sandbox}/installations/{installation}");
    let grant_count = || async {
        let (_, inst) = server
            .send(call("alice", Method::GET, &installation_path).naming(&org))
            .await;
        inst["data"]["grant_count"].clone()
    };
    assert_eq!(grant_count().await, 2);
    server
        .send(
            call(
                "alice",
                Method::DELETE,
                format!("{}/{}", agents_path(&sandbox), agents[1]),
            )
            .naming(&org),
        )
        .await;
    assert_eq!(grant_count().await, 1, "the deleted agent's grant went with it");
    let (status, _) = server
        .send(call("alice", Method::DELETE, &installation_path).naming(&org))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, grants) = server
        .send(call("alice", Method::GET, grants_path(&sandbox, &agents[0])).naming(&org))
        .await;
    assert_eq!(
        grants["data"],
        json!([]),
        "the deleted installation's grant went with it"
    );
}

#[tokio::test]
async fn lets_viewers_read_but_refuses_their_changes_before_reading_the_body() {
    let Setup {
        server,
        org,
        sandbox,
        installation,
    } = setup().await;
    server.member("alice", &org, "victor", "viewer").await;
    server.member("alice", &org, "dana", "developer").await;
    let agent = create_agent(&server, "dana", &org, &sandbox, "By a developer").await;
    let agent_path = format!("{}/{}", agents_path(&sandbox), id(&agent));
    let (status, grant) = server
        .send(
            call("dana", Method::POST, grants_path(&sandbox, id(&agent)))
                .naming(&org)
                .body(json!({ "installation_id": installation, "tools": ["search"], "approval_required": [] })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let grant_path = format!("{}/{}", grants_path(&sandbox, id(&agent)), id(&grant["data"]));
    for path in [
        agents_path(&sandbox),
        agent_path.clone(),
        grants_path(&sandbox, id(&agent)),
    ] {
        let (status, _) = server.send(call("victor", Method::GET, path).naming(&org)).await;
        assert_eq!(status, StatusCode::OK);
    }
    // Not even valid JSON: the role answers first.
    for (method, path) in [
        (Method::POST, agents_path(&sandbox)),
        (Method::PATCH, agent_path.clone()),
        (Method::DELETE, agent_path.clone()),
        (Method::POST, grants_path(&sandbox, id(&agent))),
        (Method::PATCH, grant_path.clone()),
        (Method::DELETE, grant_path.clone()),
    ] {
        let (status, body) = server
            .send(
                call("victor", method.clone(), &path)
                    .naming(&org)
                    .body(json!("not an object")),
            )
            .await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::FORBIDDEN, "forbidden"),
            "{method} {path}"
        );
    }
    let (_, grants) = server
        .send(call("alice", Method::GET, grants_path(&sandbox, id(&agent))).naming(&org))
        .await;
    assert_eq!(grants["data"].as_array().unwrap().len(), 1, "nothing changed");
}

#[tokio::test]
async fn keeps_agents_and_grants_to_their_organization_workspace_and_environment() {
    let Setup {
        server,
        org,
        sandbox,
        installation,
    } = setup().await;
    let sibling = id(&server.workspace("alice", &org, "qa-playground", "sandbox").await).to_owned();
    let production = id(&server.workspace("alice", &org, "acme", "production").await).to_owned();
    let other_org = server.organization("riley", "larkspur").await;
    let other_ws = id(&server.workspace("riley", &other_org, "harbor", "sandbox").await).to_owned();
    let agent = create_agent(&server, "alice", &org, &sandbox, "Scoped").await;
    let (_, grant) = server
        .send(
            call("alice", Method::POST, grants_path(&sandbox, id(&agent)))
                .naming(&org)
                .body(json!({ "installation_id": installation, "tools": ["search"], "approval_required": [] })),
        )
        .await;
    let grant = id(&grant["data"]).to_owned();
    let sibling_agent = create_agent(&server, "alice", &org, &sibling, "Sibling").await;

    // Real IDs, wrong scope.
    for (user, named, ws) in [
        ("alice", org.as_str(), sibling.as_str()),
        ("riley", other_org.as_str(), sandbox.as_str()),
        ("riley", other_org.as_str(), other_ws.as_str()),
        ("riley", org.as_str(), sandbox.as_str()),
    ] {
        let agent_path = format!("{}/{}", agents_path(ws), id(&agent));
        let grant_path = format!("{}/{grant}", grants_path(ws, id(&agent)));
        let change = json!({ "tools": ["search"], "approval_required": [] });
        for (method, path, body) in [
            (Method::GET, agent_path.clone(), None),
            (Method::PATCH, agent_path.clone(), Some(json!({ "status": "paused" }))),
            (Method::DELETE, agent_path.clone(), None),
            (Method::GET, grants_path(ws, id(&agent)), None),
            (Method::PATCH, grant_path.clone(), Some(change.clone())),
            (Method::DELETE, grant_path.clone(), None),
        ] {
            let mut request = call(user, method.clone(), &path).naming(named);
            if let Some(body) = body {
                request = request.body(body);
            }
            let (status, body) = server.send(request).await;
            assert_eq!(
                (status, code(&body)),
                (StatusCode::NOT_FOUND, "not_found"),
                "{user} {method} {path}"
            );
        }
    }
    // Another workspace's installation can't be granted, nor another workspace's agent reached.
    let (status, body) = server
        .send(
            call("alice", Method::POST, grants_path(&sibling, id(&sibling_agent)))
                .naming(&org)
                .body(json!({ "installation_id": installation, "tools": ["search"], "approval_required": [] })),
        )
        .await;
    assert_eq!((status, code(&body)), (StatusCode::NOT_FOUND, "not_found"));

    // An API key acts in its own organization and environment only.
    let sandbox_key = api_key(&server, "alice", &org, "sandbox").await;
    let production_key = api_key(&server, "alice", &org, "production").await;
    let other_key = api_key(&server, "riley", &other_org, "sandbox").await;
    let (status, created) = server
        .send(call(&sandbox_key, Method::POST, agents_path(&sandbox)).body(json!({
            "name": "By a key", "description": "", "model": "model-a", "instructions": "",
        })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let (status, _) = server
        .send(
            call(&production_key, Method::POST, agents_path(&production)).body(json!({
                "name": "Live", "description": "", "model": "model-a", "instructions": "",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    for (key, ws) in [
        (&production_key, &sandbox),
        (&sandbox_key, &production),
        (&other_key, &sandbox),
    ] {
        for (method, path) in [
            (Method::GET, agents_path(ws)),
            (Method::GET, format!("{}/{}", agents_path(ws), id(&agent))),
            (Method::GET, grants_path(ws, id(&agent))),
            (Method::DELETE, format!("{}/{grant}", grants_path(ws, id(&agent)))),
        ] {
            let (status, body) = server.send(call(key, method.clone(), &path)).await;
            assert_eq!(
                (status, code(&body)),
                (StatusCode::NOT_FOUND, "not_found"),
                "{method} {path}"
            );
        }
    }
    let (_, grants) = server
        .send(call("alice", Method::GET, grants_path(&sandbox, id(&agent))).naming(&org))
        .await;
    assert_eq!(grants["data"].as_array().unwrap().len(), 1, "nothing changed");
}

#[tokio::test]
async fn replays_a_created_agent_for_the_same_idempotency_key() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let ws = id(&server.workspace("alice", &org, "acme-sandbox", "sandbox").await).to_owned();
    let request = || {
        call("alice", Method::POST, agents_path(&ws))
            .naming(&org)
            .idempotent("agent-1")
            .body(json!({ "name": "Once", "description": "", "model": "model-a", "instructions": "" }))
    };
    let (first_status, first) = server.send(request()).await;
    let (again_status, again) = server.send(request()).await;
    assert_eq!((first_status, again_status), (StatusCode::CREATED, StatusCode::CREATED));
    assert_eq!(first, again);
    let (_, list) = server
        .send(call("alice", Method::GET, agents_path(&ws)).naming(&org))
        .await;
    assert_eq!(list["data"].as_array().unwrap().len(), 1);
}
