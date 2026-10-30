//! Agents and grants through the API (decision 0040): any member reads them,
//! developers and stricter change them, an API key acts in its own
//! organization and environment; a grant names only tools its installation's
//! registry holds and doesn't hold as gone; an agent names no model yet.

#![allow(clippy::unwrap_used)]

mod common;

use axum::http::{Method, StatusCode};
use common::{Server, call, code};
use serde_json::{Value, json};
use zupflow_core::apps::ToolEffect;
use zupflow_core::ids::{InstallationId, OrganizationId, WorkspaceId};
use zupflow_core::time::Timestamp;
use zupflow_core::tools::{ListedTool, ToolRevision};
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
/// as listed, `archive` changed since and `drop` gone, each allowed by the
/// organization.
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
    // The organization allows every tool, so the grant's own rules decide here.
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::PATCH,
                format!("/v1/organizations/{org}/apps/{app}/tool-levels"),
            )
            .body(json!({
                "levels": [
                    { "tool": "search", "level": "allow" },
                    { "tool": "archive", "level": "allow" },
                    { "tool": "drop", "level": "allow" },
                ],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
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
                .body(json!({ "name": name, "description": "", "instructions": "" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["data"].clone()
}

/// The deployment-wide model list left the contract (decision 0050): models
/// are each organization's own now.
#[tokio::test]
async fn no_longer_lists_deployment_models() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let key = api_key(&server, "alice", &org, "sandbox").await;
    for who in ["alice", key.as_str()] {
        let (status, body) = server.send(call(who, Method::GET, "/v1/models")).await;
        assert_eq!((status, code(&body)), (StatusCode::NOT_FOUND, "not_found"), "{body}");
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
            "name": "  Publisher  ", "description": " Drafts posts. ", "model_id": null, "instructions": "  Be brief.\n",
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
            &agent["model_id"],
            &agent["instructions"],
        ),
        (
            &json!(ws),
            &json!("Publisher"),
            &json!("Drafts posts."),
            &Value::Null,
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
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let ws = id(&server.workspace("alice", &org, "acme-sandbox", "sandbox").await).to_owned();
    let fields_of = |body: &Value| -> Vec<String> {
        body["error"]["fields"]
            .as_object()
            .map(|fields| fields.keys().cloned().collect())
            .unwrap_or_default()
    };
    for (body, invalid) in [
        (json!({}), vec!["description", "instructions", "name"]),
        (
            json!({ "name": "   ", "description": "", "instructions": "" }),
            vec!["name"],
        ),
        (
            json!({ "name": "x".repeat(61), "description": "d".repeat(281), "instructions": "i".repeat(20_001) }),
            vec!["description", "instructions", "name"],
        ),
        (
            json!({ "name": 1, "description": null, "instructions": "" }),
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

    // No organization has models here yet: naming one is refused, on create
    // and on change, and the agent keeps the environment's default.
    let unknown =
        json!({ "name": "A", "description": "", "instructions": "", "model_id": "mdl_0000000000000000000000" });
    for (method, target, body) in [
        (Method::POST, agents_path(&ws), unknown.clone()),
        (
            Method::PATCH,
            path.clone(),
            json!({ "model_id": "mdl_0000000000000000000000" }),
        ),
        (Method::PATCH, path.clone(), json!({ "model_id": "not-an-id" })),
    ] {
        let (status, answer) = server
            .send(call("alice", method, &target).naming(&org).body(body.clone()))
            .await;
        assert_eq!(
            (status, code(&answer), fields_of(&answer)),
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "unknown_model",
                vec!["model_id".to_owned()]
            ),
            "{body}"
        );
    }
    let (status, answer) = server
        .send(
            call("alice", Method::PATCH, &path)
                .naming(&org)
                .body(json!({ "model_id": null })),
        )
        .await;
    assert_eq!((status, &answer["data"]["model_id"]), (StatusCode::OK, &Value::Null));

    // A model added in Sandbox is the agent's to use; one added in
    // Production isn't, here, and nothing falls back to it.
    let sandbox_model = server.default_model("alice", &org, "sandbox").await;
    let production_model = server.default_model("alice", &org, "production").await;
    let (status, answer) = server
        .send(
            call("alice", Method::PATCH, &path)
                .naming(&org)
                .body(json!({ "model_id": production_model })),
        )
        .await;
    assert_eq!(
        (status, code(&answer)),
        (StatusCode::UNPROCESSABLE_ENTITY, "unknown_model")
    );
    let (status, answer) = server
        .send(
            call("alice", Method::PATCH, &path)
                .naming(&org)
                .body(json!({ "model_id": sandbox_model })),
        )
        .await;
    assert_eq!(
        (status, &answer["data"]["model_id"]),
        (StatusCode::OK, &json!(sandbox_model))
    );
    let (status, answer) = server
        .send(call("alice", Method::POST, agents_path(&ws)).naming(&org).body(json!({
            "name": "B", "description": "", "instructions": "", "model_id": sandbox_model,
        })))
        .await;
    assert_eq!(
        (status, &answer["data"]["model_id"]),
        (StatusCode::CREATED, &json!(sandbox_model))
    );

    // The model an agent holds stays until the agent lets it go.
    server
        .send(
            call(
                "alice",
                Method::PUT,
                format!("/v1/organizations/{org}/default-models/sandbox"),
            )
            .body(json!({ "model_id": null })),
        )
        .await;
    let model_path = format!("/v1/organizations/{org}/models/{sandbox_model}");
    let (status, answer) = server.send(call("alice", Method::DELETE, &model_path)).await;
    assert_eq!(
        (status, code(&answer)),
        (StatusCode::CONFLICT, "model_in_use"),
        "{answer}"
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
            "name": "By a key", "description": "", "instructions": "",
        })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let (status, _) = server
        .send(
            call(&production_key, Method::POST, agents_path(&production)).body(json!({
                "name": "Live", "description": "", "instructions": "",
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
            .body(json!({ "name": "Once", "description": "", "instructions": "" }))
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

/// An agent's memory (`memory`, with `memory_type_id`): the organization's
/// default type, one of its types in any environment, or none. A change
/// applies what it sends to the agent's own, and the agent ends on `type`
/// with a type or on another memory with none.
#[tokio::test]
async fn keeps_an_agent_on_the_default_unless_it_names_one_of_its_organizations_types_or_none() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let ws = id(&server.workspace("alice", &org, "acme-sandbox", "sandbox").await).to_owned();
    let production = id(&server.workspace("alice", &org, "acme", "production").await).to_owned();
    let mut types = Vec::new();
    for name in ["First", "Second"] {
        let (status, made) = server
            .send(
                call("alice", Method::POST, format!("/v1/organizations/{org}/memory-types"))
                    .body(json!({ "name": name, "kind": "notes" })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
        types.push(id(&made["data"]).to_owned());
    }
    let (first, second) = (types[0].as_str(), types[1].as_str());
    let memory_of = |agent: &Value| (agent["memory"].clone(), agent["memory_type_id"].clone());
    let refusal = |status: StatusCode, body: &Value| {
        let fields: Vec<String> = body["error"]["fields"]
            .as_object()
            .map(|fields| fields.keys().cloned().collect())
            .unwrap_or_default();
        (status, code(body).to_owned(), fields)
    };
    let invalid = |field: &str| {
        (
            StatusCode::BAD_REQUEST,
            "validation_failed".to_owned(),
            vec![field.to_owned()],
        )
    };
    let unknown = (
        StatusCode::UNPROCESSABLE_ENTITY,
        "unknown_memory_type".to_owned(),
        vec!["memory_type_id".to_owned()],
    );
    let create = |ws: &str, name: &str, memory: Value| {
        let mut body = json!({ "name": name, "description": "", "instructions": "" });
        body.as_object_mut()
            .unwrap()
            .extend(memory.as_object().unwrap().clone());
        call("alice", Method::POST, agents_path(ws)).naming(&org).body(body)
    };

    let plain = create_agent(&server, "alice", &org, &ws, "Plain").await;
    assert_eq!(memory_of(&plain), (json!("default"), Value::Null));
    let (status, off) = server
        .send(create(&ws, "Off", json!({ "memory": "off", "memory_type_id": null })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{off}");
    let off = off["data"].clone();
    assert_eq!(memory_of(&off), (json!("off"), Value::Null));
    let (status, typed) = server
        .send(create(
            &ws,
            "Typed",
            json!({ "memory": "type", "memory_type_id": first }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{typed}");
    let typed = typed["data"].clone();
    assert_eq!(memory_of(&typed), (json!("type"), json!(first)));
    let typed_path = format!("{}/{}", agents_path(&ws), id(&typed));
    assert_eq!(
        server
            .send(call("alice", Method::GET, &typed_path).naming(&org))
            .await
            .1["data"],
        typed
    );
    // Types are the organization's, the same in each environment.
    let (status, elsewhere) = server
        .send(create(
            &production,
            "Elsewhere",
            json!({ "memory": "type", "memory_type_id": first }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{elsewhere}");
    assert_eq!(agent_counts(&server, &org, &types).await, [2, 0]);

    // `type` needs a memory type, and a memory type needs `type`.
    for (memory, expected) in [
        (json!({ "memory": "type" }), invalid("memory_type_id")),
        (
            json!({ "memory": "type", "memory_type_id": null }),
            invalid("memory_type_id"),
        ),
        (json!({ "memory_type_id": first }), invalid("memory_type_id")),
        (
            json!({ "memory": "default", "memory_type_id": first }),
            invalid("memory_type_id"),
        ),
        (
            json!({ "memory": "off", "memory_type_id": first }),
            invalid("memory_type_id"),
        ),
        (json!({ "memory_type_id": 5 }), invalid("memory_type_id")),
        (json!({ "memory": "sometimes" }), invalid("memory")),
        (json!({ "memory": null }), invalid("memory")),
        (
            json!({ "memory": "type", "memory_type_id": "mtype_0000000000000000000000" }),
            unknown.clone(),
        ),
        (
            json!({ "memory": "type", "memory_type_id": "not-a-memory-type" }),
            unknown.clone(),
        ),
    ] {
        let (status, body) = server.send(create(&ws, "Refused", memory.clone())).await;
        assert_eq!(refusal(status, &body), expected, "{memory}");
    }
    let (_, listed) = server
        .send(call("alice", Method::GET, agents_path(&ws)).naming(&org))
        .await;
    assert!(
        listed["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|agent| agent["name"] != "Refused"),
        "no refused agent made"
    );

    // A change keeps what it doesn't send; a type sent alone moves an agent
    // on `type` to it; `off` and `default` let go of the type.
    let change = |path: &str, body: Value| call("alice", Method::PATCH, path).naming(&org).body(body);
    for (body, expected) in [
        (json!({ "name": "Renamed" }), (json!("type"), json!(first))),
        (json!({ "memory": "type" }), (json!("type"), json!(first))),
        (json!({ "memory_type_id": second }), (json!("type"), json!(second))),
    ] {
        let (status, changed) = server.send(change(&typed_path, body.clone())).await;
        assert_eq!(
            (status, memory_of(&changed["data"])),
            (StatusCode::OK, expected),
            "{body}"
        );
    }
    assert_eq!(agent_counts(&server, &org, &types).await, [1, 1]);
    let (status, cleared) = server.send(change(&typed_path, json!({ "memory": "off" }))).await;
    assert_eq!(
        (status, memory_of(&cleared["data"])),
        (StatusCode::OK, (json!("off"), Value::Null))
    );
    assert_eq!(agent_counts(&server, &org, &types).await, [1, 0]);
    server
        .send(change(
            &typed_path,
            json!({ "memory": "type", "memory_type_id": first }),
        ))
        .await;
    let (status, back) = server
        .send(change(
            &typed_path,
            json!({ "memory": "default", "memory_type_id": null }),
        ))
        .await;
    assert_eq!(
        (status, memory_of(&back["data"])),
        (StatusCode::OK, (json!("default"), Value::Null))
    );

    // A refused change changes nothing.
    let plain_path = format!("{}/{}", agents_path(&ws), id(&plain));
    let (_, on_type) = server
        .send(change(
            &plain_path,
            json!({ "memory": "type", "memory_type_id": first }),
        ))
        .await;
    let on_type = on_type["data"].clone();
    let off_path = format!("{}/{}", agents_path(&ws), id(&off));
    for (path, body, expected) in [
        (&off_path, json!({ "memory_type_id": first }), invalid("memory_type_id")),
        (&off_path, json!({ "memory": "type" }), invalid("memory_type_id")),
        (
            &off_path,
            json!({ "memory": "default", "memory_type_id": first }),
            invalid("memory_type_id"),
        ),
        (
            &plain_path,
            json!({ "memory_type_id": null }),
            invalid("memory_type_id"),
        ),
        (
            &plain_path,
            json!({ "memory": "type", "memory_type_id": null }),
            invalid("memory_type_id"),
        ),
        (
            &plain_path,
            json!({ "memory_type_id": "mtype_0000000000000000000000" }),
            unknown.clone(),
        ),
        (
            &plain_path,
            json!({ "memory_type_id": "not-a-memory-type" }),
            unknown.clone(),
        ),
    ] {
        let (status, answer) = server.send(change(path, body.clone())).await;
        assert_eq!(refusal(status, &answer), expected, "{path} {body}");
    }

    // In order: the model, then the memory, then its type, one of no type's
    // form included.
    let unknown_model = (
        StatusCode::UNPROCESSABLE_ENTITY,
        "unknown_model".to_owned(),
        vec!["model_id".to_owned()],
    );
    for (memory, expected) in [
        (
            json!({ "memory": "type", "memory_type_id": null }),
            invalid("memory_type_id"),
        ),
        (
            json!({ "memory": "default", "memory_type_id": first }),
            invalid("memory_type_id"),
        ),
        (
            json!({ "memory": "off", "memory_type_id": "not-a-memory-type" }),
            invalid("memory_type_id"),
        ),
        (
            json!({ "memory": "type", "memory_type_id": "mtype_0000000000000000000000" }),
            unknown.clone(),
        ),
        (
            json!({ "memory": "type", "memory_type_id": "not-a-memory-type" }),
            unknown.clone(),
        ),
    ] {
        let mut with_model = memory.clone();
        with_model["model_id"] = json!("mdl_0000000000000000000000");
        let (status, answer) = server.send(create(&ws, "Refused", with_model.clone())).await;
        assert_eq!(refusal(status, &answer), unknown_model, "made with {with_model}");
        let (status, answer) = server.send(create(&ws, "Refused", memory.clone())).await;
        assert_eq!(refusal(status, &answer), expected, "made with {memory}");
        for path in [&plain_path, &off_path] {
            let (status, answer) = server.send(change(path, with_model.clone())).await;
            assert_eq!(refusal(status, &answer), unknown_model, "{path} {with_model}");
            let (status, answer) = server.send(change(path, memory.clone())).await;
            assert_eq!(refusal(status, &answer), expected, "{path} {memory}");
        }
    }
    let read = |path: &str| server.send(call("alice", Method::GET, path).naming(&org));
    assert_eq!(read(&plain_path).await.1["data"], on_type);
    assert_eq!(read(&off_path).await.1["data"], off);
}

/// How many agents name each of `types`.
async fn agent_counts(server: &Server, org: &str, types: &[String]) -> Vec<u64> {
    let mut counts = Vec::new();
    for memory_type in types {
        let (_, read) = server
            .send(call(
                "alice",
                Method::GET,
                format!("/v1/organizations/{org}/memory-types/{memory_type}"),
            ))
            .await;
        counts.push(read["data"]["agent_count"].as_u64().unwrap());
    }
    counts
}

/// Two changes of an agent's memory at once apply one after the other, each
/// to the agent as the other left it, never to a copy read before the other
/// wrote. Whichever goes first, an agent on a type turned off at once ends
/// off: a type sent alone, or `type` with no type, applied to an agent
/// already off is refused for it.
#[tokio::test(flavor = "multi_thread")]
async fn applies_memory_changes_made_at_once_one_after_the_other() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let ws = id(&server.workspace("alice", &org, "acme-sandbox", "sandbox").await).to_owned();
    let mut types = Vec::new();
    for name in ["First", "Second"] {
        let (status, made) = server
            .send(
                call("alice", Method::POST, format!("/v1/organizations/{org}/memory-types"))
                    .body(json!({ "name": name, "kind": "notes" })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
        types.push(id(&made["data"]).to_owned());
    }
    let (first, second) = (types[0].as_str(), types[1].as_str());
    let change = |agent: &str, body: Value| {
        call("alice", Method::PATCH, format!("{}/{agent}", agents_path(&ws)))
            .naming(&org)
            .body(body)
    };
    let mut ended_on_a_type = Vec::new();
    for round in 0..25 {
        for other in [
            json!({ "memory_type_id": second }),
            json!({ "name": "Kept", "memory": "type" }),
        ] {
            let (status, made) = server
                .send(call("alice", Method::POST, agents_path(&ws)).naming(&org).body(json!({
                    "name": "Racer",
                    "description": "",
                    "instructions": "",
                    "memory": "type",
                    "memory_type_id": first,
                })))
                .await;
            assert_eq!(status, StatusCode::CREATED, "{made}");
            let agent = id(&made["data"]).to_owned();
            let ((off, turned_off), (status, answer)) = tokio::join!(
                server.send(change(&agent, json!({ "memory": "off" }))),
                server.send(change(&agent, other.clone())),
            );
            assert_eq!(off, StatusCode::OK, "{turned_off}");
            assert!(
                status == StatusCode::OK
                    || (status == StatusCode::BAD_REQUEST && answer["error"]["fields"]["memory_type_id"].is_string()),
                "round {round}, {other}: {status} {answer}"
            );
            let (_, read) = server
                .send(call("alice", Method::GET, format!("{}/{agent}", agents_path(&ws))).naming(&org))
                .await;
            if (&read["data"]["memory"], &read["data"]["memory_type_id"]) != (&json!("off"), &Value::Null) {
                ended_on_a_type.push(format!("round {round}, {other}"));
            }
        }
    }
    assert!(
        ended_on_a_type.is_empty(),
        "{} of 50 agents turned off ended on a type: {ended_on_a_type:?}",
        ended_on_a_type.len()
    );
}
