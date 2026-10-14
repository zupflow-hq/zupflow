//! Tool permissions through the API (decision 0040): the organization's
//! policy and levels, read by every member and its API keys and changed only
//! by owners and admins from the Console; levels suggested by discovery and
//! in force once confirmed; grants held inside them; a changed tool's review.

#![allow(clippy::unwrap_used)]

mod common;

use axum::http::{Method, StatusCode};
use common::{Server, call, code};
use serde_json::{Value, json};
use zupflow_server::discovery::work_once;

const MCP: &str = "https://tools.example/mcp";

fn id(value: &Value) -> &str {
    value["id"].as_str().unwrap()
}

fn policy_path(org: &str) -> String {
    format!("/v1/organizations/{org}/tool-policy")
}

fn levels_path(org: &str, app: &str) -> String {
    format!("/v1/organizations/{org}/apps/{app}/tool-levels")
}

/// A catalog token app served at [`MCP`] in Sandbox.
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

/// Connects an installation through hosted Connect and discovers its tools; its ID.
async fn connect_token(server: &Server, org: &str, ws: &str, app: &str) -> String {
    let (status, session) = server
        .send(
            call("alice", Method::POST, format!("/v1/workspaces/{ws}/connect-sessions"))
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

async fn api_key(server: &Server, org: &str) -> String {
    let (_, body) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/api-keys"))
                .body(json!({ "name": "Server", "environment": "sandbox" })),
        )
        .await;
    body["data"]["secret"].as_str().unwrap().to_owned()
}

/// An organization with a viewer `vic`, a developer `bob` and an admin
/// `ada`; a Sandbox workspace with an installation of a catalog app that
/// listed `search` (read-only), `send` (no hints) and `drop` (destructive).
struct Setup {
    server: Server,
    org: String,
    sandbox: String,
    app: String,
    installation: String,
}

async fn setup() -> Setup {
    let server = Server::new().await;
    server.providers.serve_mcp(
        MCP,
        json!([
            { "name": "search", "inputSchema": { "type": "object" }, "annotations": { "readOnlyHint": true } },
            { "name": "send", "inputSchema": { "type": "object" } },
            { "name": "drop", "inputSchema": { "type": "object" }, "annotations": { "destructiveHint": true } },
        ]),
    );
    let org = server.organization("alice", "northwind").await;
    for (user, role) in [("vic", "viewer"), ("bob", "developer"), ("ada", "admin")] {
        server.member("alice", &org, user, role).await;
    }
    let sandbox = id(&server.workspace("alice", &org, "acme-sandbox", "sandbox").await).to_owned();
    let app = catalog_token_app(&server, &org).await;
    let installation = connect_token(&server, &org, &sandbox, &app).await;
    Setup {
        server,
        org,
        sandbox,
        app,
        installation,
    }
}

/// Each level as (tool, level, confirmed, destructive).
fn levels(body: &Value) -> Vec<(String, String, bool, bool)> {
    body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|level| {
            (
                level["tool"].as_str().unwrap().to_owned(),
                level["level"].as_str().unwrap().to_owned(),
                level["confirmed"].as_bool().unwrap(),
                level["destructive"].as_bool().unwrap(),
            )
        })
        .collect()
}

fn level(tool: &str, level: &str, confirmed: bool, destructive: bool) -> (String, String, bool, bool) {
    (tool.to_owned(), level.to_owned(), confirmed, destructive)
}

#[tokio::test]
async fn reads_the_policy_to_every_member_and_lets_only_admins_change_it() {
    let Setup { server, org, .. } = setup().await;
    let key = api_key(&server, &org).await;
    let (status, read) = server.send(call("vic", Method::GET, policy_path(&org))).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(
        read["data"],
        json!({ "organization_id": org, "mode": "manual", "new_tool_level": "approve", "updated_at": null, "updated_by": null })
    );
    let (status, by_key) = server.send(call(&key, Method::GET, policy_path(&org))).await;
    assert_eq!((status, &by_key["data"]), (StatusCode::OK, &read["data"]));

    // Refused before the body is read.
    let (status, body) = server
        .send(call("bob", Method::PUT, policy_path(&org)).body(json!("not an object")))
        .await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, "forbidden"));
    let (status, body) = server
        .send(call(&key, Method::PUT, policy_path(&org)).body(json!({ "mode": "manual", "new_tool_level": "deny" })))
        .await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, "console_only"));
    for (input, field) in [
        (json!({ "mode": "manual", "new_tool_level": "allow" }), "new_tool_level"),
        (json!({ "mode": "auto", "new_tool_level": "deny" }), "mode"),
        (json!({ "new_tool_level": "deny" }), "mode"),
    ] {
        let (status, body) = server
            .send(call("ada", Method::PUT, policy_path(&org)).body(input))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["error"]["fields"][field].is_string(), "{body}");
    }
    let (status, saved) = server
        .send(call("ada", Method::PUT, policy_path(&org)).body(json!({ "mode": "manual", "new_tool_level": "deny" })))
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["data"]["new_tool_level"], "deny");
    assert_eq!(saved["data"]["updated_by"]["email"], "ada@people.example");
    assert!(saved["data"]["updated_at"].is_string());
    let (_, read) = server.send(call("vic", Method::GET, policy_path(&org))).await;
    assert_eq!(read["data"], saved["data"]);

    // Another organization's member, or key, finds nothing.
    let other = server.organization("riley", "larkspur").await;
    let other_key = api_key_for(&server, "riley", &other).await;
    for who in ["riley", other_key.as_str()] {
        let (status, _) = server.send(call(who, Method::GET, policy_path(&org))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{who}");
    }
    let (status, _) = server
        .send(
            call("riley", Method::PUT, policy_path(&org))
                .body(json!({ "mode": "manual", "new_tool_level": "approve" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn api_key_for(server: &Server, user: &str, org: &str) -> String {
    let (_, body) = server
        .send(
            call(user, Method::POST, format!("/v1/organizations/{org}/api-keys"))
                .body(json!({ "name": "Server", "environment": "sandbox" })),
        )
        .await;
    body["data"]["secret"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn suggests_levels_on_discovery_and_sets_them_confirmed() {
    let Setup { server, org, app, .. } = setup().await;
    let key = api_key(&server, &org).await;
    let (status, listed) = server.send(call("vic", Method::GET, levels_path(&org, &app))).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(
        levels(&listed),
        [
            level("drop", "approve", false, true),
            level("search", "allow", false, false),
            // No hints: MCP reads it as destructive.
            level("send", "approve", false, true),
        ]
    );
    assert_eq!(listed["meta"]["pagination"]["total"], 3);
    let (_, first) = server
        .send(call(&key, Method::GET, format!("{}?limit=2", levels_path(&org, &app))))
        .await;
    let cursor = first["meta"]["pagination"]["next_cursor"].as_str().unwrap().to_owned();
    let (_, second) = server
        .send(call(
            &key,
            Method::GET,
            format!("{}?limit=2&cursor={cursor}", levels_path(&org, &app)),
        ))
        .await;
    assert_eq!(
        [levels(&first), levels(&second)].concat(),
        levels(&listed),
        "pages by name"
    );
    let (status, _) = server
        .send(call(
            &key,
            Method::GET,
            format!("{}?cursor=%%%", levels_path(&org, &app)),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let change = json!({ "levels": [{ "tool": "send", "level": "allow" }, { "tool": "ahead", "level": "deny" }] });
    let (status, body) = server
        .send(call("bob", Method::PATCH, levels_path(&org, &app)).body(json!(null)))
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "forbidden"),
        "before the body"
    );
    let (status, body) = server
        .send(call(&key, Method::PATCH, levels_path(&org, &app)).body(change.clone()))
        .await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, "console_only"));
    for levels in [
        json!([]),
        json!([{ "tool": "send", "level": "allow" }, { "tool": "send", "level": "deny" }]),
        json!([{ "tool": "", "level": "allow" }]),
        json!([{ "tool": "x".repeat(129), "level": "allow" }]),
        json!([{ "tool": "send", "level": "maybe" }]),
        json!(
            (0..101)
                .map(|i| json!({ "tool": format!("t{i}"), "level": "allow" }))
                .collect::<Vec<_>>()
        ),
        json!("send"),
    ] {
        let (status, body) = server
            .send(call("ada", Method::PATCH, levels_path(&org, &app)).body(json!({ "levels": levels })))
            .await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::BAD_REQUEST, "validation_failed"),
            "{body}"
        );
        assert!(body["error"]["fields"]["levels"].is_string(), "{body}");
    }
    let (status, set) = server
        .send(call("ada", Method::PATCH, levels_path(&org, &app)).body(change))
        .await;
    assert_eq!(status, StatusCode::OK, "{set}");
    assert_eq!(
        levels(&set),
        [level("ahead", "deny", true, false), level("send", "allow", true, true)]
    );
    assert_eq!(set["data"][0]["updated_by"]["email"], "ada@people.example");
    let (_, listed) = server.send(call("vic", Method::GET, levels_path(&org, &app))).await;
    assert_eq!(levels(&listed).len(), 4);

    // Unknown and malformed apps, and another organization's, are not found.
    let other = server.organization("riley", "larkspur").await;
    let other_app = catalog_token_app_as(&server, "riley", &other).await;
    for path in [
        levels_path(&org, "app_0190a8b0-0000-7000-8000-000000000000"),
        levels_path(&org, "nonsense"),
        levels_path(&org, &other_app),
    ] {
        let (status, _) = server.send(call("alice", Method::GET, path.clone())).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        let (status, _) = server
            .send(call("alice", Method::PATCH, path.clone()).body(json!({ "levels": [] })))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}, before the body");
    }
}

async fn catalog_token_app_as(server: &Server, user: &str, org: &str) -> String {
    let (status, created) = server
        .send(
            call(user, Method::POST, format!("/v1/organizations/{org}/apps")).body(json!({
                "kind": "mcp", "name": "Theirs", "key": "theirs", "description": "", "auth_type": "token",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    id(&created["data"]).to_owned()
}

#[tokio::test]
async fn holds_grants_inside_the_levels() {
    let Setup {
        server,
        org,
        sandbox,
        app,
        installation,
    } = setup().await;
    let (_, agent) = server
        .send(
            call("alice", Method::POST, format!("/v1/workspaces/{sandbox}/agents"))
                .naming(&org)
                .body(json!({ "name": "Held", "description": "", "instructions": "" })),
        )
        .await;
    let grants = format!("/v1/workspaces/{sandbox}/agents/{}/grants", id(&agent["data"]));
    let grant = |tools: &[&str], approvals: &[&str]| {
        call("bob", Method::POST, grants.clone())
            .naming(&org)
            .body(json!({ "installation_id": installation, "tools": tools, "approval_required": approvals }))
    };
    // Nothing confirmed: every tool waits for approval, as new tools do.
    let (status, body) = server.send(grant(&["search"], &[])).await;
    assert_eq!(
        (status, code(&body), &body["error"]["fields"]["approval_required"]),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "approval_required_by_policy",
            &json!("Your organization requires approval for search. Add it to approval required.")
        )
    );
    let (status, _) = server
        .send(
            call("alice", Method::PATCH, levels_path(&org, &app)).body(json!({ "levels": [
                { "tool": "search", "level": "allow" },
                { "tool": "send", "level": "approve" },
                { "tool": "drop", "level": "deny" },
            ]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = server.send(grant(&["search", "drop"], &[])).await;
    assert_eq!(
        (status, code(&body), &body["error"]["fields"]["tools"]),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "tool_denied",
            &json!("Your organization blocks drop. Remove it from the grant.")
        )
    );
    let (status, body) = server.send(grant(&["missing", "drop"], &[])).await;
    assert_eq!(code(&body), "unknown_tool", "the registry's rules first: {status}");
    let (status, body) = server.send(grant(&["search", "send"], &[])).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "approval_required_by_policy")
    );
    let (status, made) = server.send(grant(&["search", "send"], &["send"])).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");

    // Tightening rewrites no grant; a change to the grant is held to it.
    let (status, _) = server
        .send(
            call("alice", Method::PATCH, levels_path(&org, &app)).body(json!({ "levels": [
                { "tool": "search", "level": "deny" },
            ]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, listed) = server.send(call("vic", Method::GET, grants.clone()).naming(&org)).await;
    assert_eq!(listed["data"], json!([made["data"]]));
    let (status, body) = server
        .send(
            call("bob", Method::PATCH, format!("{grants}/{}", id(&made["data"])))
                .naming(&org)
                .body(json!({ "tools": ["search", "send"], "approval_required": ["send"] })),
        )
        .await;
    assert_eq!((status, code(&body)), (StatusCode::UNPROCESSABLE_ENTITY, "tool_denied"));
}

#[tokio::test]
async fn accepts_a_changed_tool_exactly_as_reviewed() {
    let Setup {
        server,
        org,
        sandbox,
        installation,
        ..
    } = setup().await;
    // Listed again: `send` changed.
    server.providers.serve_mcp(
        MCP,
        json!([
            { "name": "search", "inputSchema": { "type": "object" }, "annotations": { "readOnlyHint": true } },
            { "name": "send", "description": "Send to everyone", "inputSchema": { "type": "object" } },
            { "name": "drop", "inputSchema": { "type": "object" }, "annotations": { "destructiveHint": true } },
        ]),
    );
    let (status, _) = server
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/workspaces/{sandbox}/installations/{installation}/refresh-tools"),
            )
            .naming(&org),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    while work_once(&server.state).await {}
    let tools_path = format!("/v1/workspaces/{sandbox}/installations/{installation}/tools");
    let (_, tools) = server
        .send(call("vic", Method::GET, tools_path.clone()).naming(&org))
        .await;
    let find = |tools: &Value, name: &str| {
        tools["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == name)
            .cloned()
            .unwrap()
    };
    let send = find(&tools, "send");
    let latest = send["latest_hash"].as_str().unwrap().to_owned();
    let pinned = send["pinned_hash"].as_str().unwrap().to_owned();
    assert_eq!(send["status"], "changed");
    for hash in [&latest, &pinned] {
        assert!(hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    }
    assert_ne!(latest, pinned);
    assert_eq!(find(&tools, "search")["latest_hash"], Value::Null);

    let accept_path = format!("/v1/workspaces/{sandbox}/installations/{installation}/accept-tool-change");
    let accept = |who: &'static str, name: &str, hash: &str| {
        call(who, Method::POST, accept_path.clone())
            .naming(&org)
            .body(json!({ "name": name, "latest_hash": hash }))
    };
    let (status, body) = server
        .send(
            call("bob", Method::POST, accept_path.clone())
                .naming(&org)
                .body(json!(1)),
        )
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "forbidden"),
        "before the body"
    );
    for (name, hash, expected) in [
        ("send", pinned.as_str(), (StatusCode::CONFLICT, "stale_definition")),
        ("send", "not-a-hash", (StatusCode::CONFLICT, "stale_definition")),
        ("search", latest.as_str(), (StatusCode::CONFLICT, "tool_not_changed")),
        ("missing", latest.as_str(), (StatusCode::NOT_FOUND, "not_found")),
        ("", latest.as_str(), (StatusCode::BAD_REQUEST, "validation_failed")),
        ("send", "", (StatusCode::BAD_REQUEST, "validation_failed")),
    ] {
        let (status, body) = server.send(accept("ada", name, hash)).await;
        assert_eq!((status, code(&body)), expected, "{name} {hash}: {body}");
    }
    let (_, unchanged) = server
        .send(call("vic", Method::GET, tools_path.clone()).naming(&org))
        .await;
    assert_eq!(unchanged, tools, "a refused accept changes nothing");

    let (status, accepted) = server.send(accept("ada", "send", &latest)).await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    let mut expected = send.clone();
    expected["status"] = json!("current");
    expected["pinned"] = send["latest"].clone();
    expected["pinned_hash"] = json!(latest);
    expected["latest"] = Value::Null;
    expected["latest_hash"] = Value::Null;
    expected["changed_at"] = Value::Null;
    assert_eq!(accepted["data"], expected);
    let (_, after) = server.send(call("vic", Method::GET, tools_path).naming(&org)).await;
    assert_eq!(find(&after, "send"), expected);
    let (status, body) = server.send(accept("ada", "send", &latest)).await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "tool_not_changed"));

    // Another organization's member finds no installation.
    let other = server.organization("riley", "larkspur").await;
    let (status, _) = server
        .send(
            call(
                "riley",
                Method::POST,
                format!("/v1/workspaces/{sandbox}/installations/{installation}/accept-tool-change"),
            )
            .naming(&other)
            .body(json!({ "name": "send", "latest_hash": latest })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A changed tool whose definition is exactly at the registry's limit is
/// accepted, and the answer is saved for its idempotency key like any other.
#[tokio::test]
async fn accepts_a_changed_tool_at_the_definition_limit() {
    use zupflow_core::tools::MAX_DEFINITION_BYTES;

    let Setup {
        server,
        org,
        sandbox,
        installation,
        ..
    } = setup().await;
    // The canonical definition sorts its keys: description, inputSchema, name.
    let wrapper = r#"{"description":"","inputSchema":{"type":"object"},"name":"send"}"#.len();
    let description = "x".repeat(MAX_DEFINITION_BYTES - wrapper);
    server.providers.serve_mcp(
        MCP,
        json!([
            { "name": "search", "inputSchema": { "type": "object" }, "annotations": { "readOnlyHint": true } },
            { "name": "send", "description": description, "inputSchema": { "type": "object" } },
            { "name": "drop", "inputSchema": { "type": "object" }, "annotations": { "destructiveHint": true } },
        ]),
    );
    let (status, _) = server
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/workspaces/{sandbox}/installations/{installation}/refresh-tools"),
            )
            .naming(&org),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    while work_once(&server.state).await {}
    let tools_path = format!("/v1/workspaces/{sandbox}/installations/{installation}/tools");
    let (_, tools) = server.send(call("vic", Method::GET, tools_path).naming(&org)).await;
    let send = tools["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "send")
        .cloned()
        .unwrap();
    assert_eq!(send["status"], "changed");
    assert_eq!(
        send["latest"]["description"].as_str().map(str::len),
        Some(description.len())
    );
    let latest = send["latest_hash"].as_str().unwrap().to_owned();

    let accept = || {
        call(
            "ada",
            Method::POST,
            format!("/v1/workspaces/{sandbox}/installations/{installation}/accept-tool-change"),
        )
        .naming(&org)
        .idempotent("accept-at-the-limit")
        .body(json!({ "name": "send", "latest_hash": latest }))
    };
    let (status, accepted) = server.send(accept()).await;
    assert_eq!(status, StatusCode::OK, "{}", code(&accepted));
    assert_eq!(accepted["data"]["status"], "current");
    assert_eq!(accepted["data"]["pinned_hash"], json!(latest));
    assert_eq!(
        accepted["data"]["pinned"]["description"].as_str().map(str::len),
        Some(description.len())
    );
    let (status, replayed) = server.send(accept()).await;
    assert_eq!(status, StatusCode::OK, "the saved answer is replayed");
    assert_eq!(replayed, accepted);
}
