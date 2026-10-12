//! Apps end to end (decision 0031): the catalog and each workspace's own apps,
//! never reachable through each other's routes, another workspace's or another
//! organization's, and managed by admins only.

#![allow(clippy::unwrap_used)]

mod common;

use axum::http::{Method, StatusCode};
use common::{Server, call, code};
use serde_json::{Value, json};

fn app(key: &str, auth_type: &str) -> Value {
    json!({ "kind": "mcp", "name": "Tools", "key": key, "description": "  Internal tools.  ", "auth_type": auth_type })
}

/// Creates a catalog app as `user`; its body.
async fn catalog_app(server: &Server, user: &str, org: &str, key: &str) -> Value {
    let (status, body) = server
        .send(call(user, Method::POST, format!("/v1/organizations/{org}/apps")).body(app(key, "token")))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["data"].clone()
}

/// Creates a workspace app as `user`; its body.
async fn workspace_app(server: &Server, user: &str, org: &str, ws: &str, key: &str) -> Value {
    let (status, body) = server
        .send(
            call(user, Method::POST, format!("/v1/workspaces/{ws}/apps"))
                .naming(org)
                .body(app(key, "token")),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["data"].clone()
}

fn id(value: &Value) -> &str {
    value["id"].as_str().unwrap()
}

#[tokio::test]
async fn keeps_a_catalog_of_apps_in_both_environments() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let (status, body) = server
        .send(call("alice", Method::POST, format!("/v1/organizations/{org}/apps")).body(app("linear", "oauth2")))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let created = &body["data"];
    assert!(id(created).starts_with("app_"));
    assert_eq!(created["organization_id"], org.as_str());
    assert_eq!(
        (&created["owner_scope"], &created["owner_workspace_id"]),
        (&json!("organization"), &Value::Null)
    );
    assert_eq!(
        (&created["kind"], &created["key"], &created["description"]),
        (&json!("mcp"), &json!("linear"), &json!("Internal tools.")),
        "the description is trimmed"
    );
    assert_eq!(created["icon"], json!({ "glyph": "T", "color": "#15803D" }));
    assert_eq!(
        created["auth"],
        json!({
            "type": "oauth2", "authorize_url": null, "token_url": null,
            "available_scopes": [], "pkce": true, "identity_url": null, "issuer": null,
        })
    );
    assert_eq!(created["tools"], json!([]));
    assert_eq!(
        (&created["version"], &created["installation_count"]),
        (&json!(1), &json!(0))
    );
    assert_eq!(
        created["configurations"],
        json!([
            { "environment": "sandbox", "status": "unconfigured", "version": 0, "updated_at": null },
            { "environment": "production", "status": "unconfigured", "version": 0, "updated_at": null },
        ])
    );
    assert_eq!(created["created_at"], created["updated_at"]);

    let path = format!("/v1/organizations/{org}/apps/{}", id(created));
    let (status, got) = server.send(call("alice", Method::GET, &path)).await;
    assert_eq!((status, &got["data"]), (StatusCode::OK, created));

    let (status, updated) = server
        .send(call("alice", Method::PATCH, &path).body(json!({ "name": " Linear ", "description": "" })))
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(
        (
            &updated["data"]["name"],
            &updated["data"]["description"],
            &updated["data"]["key"]
        ),
        (&json!("Linear"), &json!(""), &json!("linear"))
    );
    let (_, untouched) = server.send(call("alice", Method::PATCH, &path).body(json!({}))).await;
    assert_eq!(untouched["data"]["name"], "Linear", "an empty update changes nothing");

    catalog_app(&server, "alice", &org, "acme_tools").await;
    let (status, list) = server
        .send(call("alice", Method::GET, format!("/v1/organizations/{org}/apps")))
        .await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|app| app["key"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["linear", "acme_tools"], "by name: Linear, then Tools");

    let (status, taken) = server
        .send(call("alice", Method::POST, format!("/v1/organizations/{org}/apps")).body(app("linear", "token")))
        .await;
    assert_eq!((status, code(&taken)), (StatusCode::CONFLICT, "app_key_taken"));
    assert_eq!(
        taken["error"]["fields"]["key"],
        "Already used by another app. Choose another."
    );
    // The key is fixed once created: the update input has none, and one sent anyway changes nothing.
    let (status, kept) = server
        .send(call("alice", Method::PATCH, &path).body(json!({ "key": "renamed" })))
        .await;
    assert_eq!((status, &kept["data"]["key"]), (StatusCode::OK, &json!("linear")));
}

#[tokio::test]
async fn checks_what_an_app_is_created_with() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let path = format!("/v1/organizations/{org}/apps");

    let (status, body) = server.send(call("alice", Method::POST, &path).body(json!({}))).await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, "validation_failed"));
    let fields = body["error"]["fields"].as_object().unwrap();
    let named: Vec<&str> = fields.keys().map(String::as_str).collect();
    assert_eq!(named, ["auth_type", "description", "key", "kind", "name"]);
    assert_eq!(
        fields["auth_type"],
        "Choose OAuth, sign-in through the MCP server, or token."
    );

    let mut wrong = app("ok", "basic");
    wrong["description"] = json!("x".repeat(281));
    wrong["key"] = json!("Not A Key");
    let (_, body) = server.send(call("alice", Method::POST, &path).body(wrong)).await;
    assert_eq!(
        body["error"]["fields"],
        json!({
            "auth_type": "Choose OAuth, sign-in through the MCP server, or token.",
            "description": "Use 280 characters or fewer.",
            "key": "Use lowercase letters, numbers and single underscores, starting with a letter.",
        })
    );

    // A kind that isn't `mcp` is refused, never coerced, once the rest is valid.
    let mut other = app("other", "token");
    other["kind"] = json!("openapi");
    let (status, body) = server.send(call("alice", Method::POST, &path).body(other)).await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, "unsupported_app_kind"));
    assert_eq!(body["error"]["fields"]["kind"], "Only \"mcp\" is supported.");

    // Snake case, 1 to 24 characters, starting with a letter (decision 0047).
    for (key, message) in [
        (
            "",
            "Use lowercase letters, numbers and single underscores, starting with a letter.",
        ),
        (
            "kebab-case",
            "Use lowercase letters, numbers and single underscores, starting with a letter.",
        ),
        (
            "_lead",
            "Use lowercase letters, numbers and single underscores, starting with a letter.",
        ),
        (
            "trail_",
            "Use lowercase letters, numbers and single underscores, starting with a letter.",
        ),
        (
            "two__parts",
            "Use lowercase letters, numbers and single underscores, starting with a letter.",
        ),
        (
            "9lives",
            "Use lowercase letters, numbers and single underscores, starting with a letter.",
        ),
        ("abcdefghijklmnopqrstuvwxy", "Use 24 characters or fewer."),
    ] {
        let (status, body) = server
            .send(call("alice", Method::POST, &path).body(app(key, "token")))
            .await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::BAD_REQUEST, "validation_failed"),
            "{key}"
        );
        assert_eq!(body["error"]["fields"], json!({ "key": message }), "{key}");
    }
    let longest = catalog_app(&server, "alice", &org, "abcdefghijklmnopqrstuvwx").await;
    assert_eq!(longest["key"], "abcdefghijklmnopqrstuvwx");

    let mut token = app("token_app", "token");
    token["name"] = json!("échos");
    let (status, body) = server.send(call("alice", Method::POST, &path).body(token)).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["data"]["icon"]["glyph"], "É");
    assert_eq!(body["data"]["auth"]["type"], "token");
    assert_eq!(
        body["data"]["auth"]["placement"],
        json!({ "location": "header", "name": "Authorization", "prefix": "Bearer " })
    );
    assert_eq!(body["data"]["auth"]["instructions"], "Paste an API token for échos.");

    let (status, body) = server
        .send(call("alice", Method::PATCH, format!("{path}/{}", id(&body["data"]))).body(json!({ "name": "" })))
        .await;
    assert_eq!(
        (status, &body["error"]["fields"]),
        (StatusCode::BAD_REQUEST, &json!({ "name": "Required." }))
    );
}

#[tokio::test]
async fn lets_admins_manage_apps_and_every_member_read_them() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let ws = server.workspace("alice", &org, "acme", "sandbox").await;
    let ws = ws["id"].as_str().unwrap();
    let catalog = catalog_app(&server, "alice", &org, "linear").await;
    let own = workspace_app(&server, "alice", &org, ws, "echo").await;
    server.member("alice", &org, "dana", "admin").await;
    server.member("alice", &org, "devin", "developer").await;
    server.member("alice", &org, "vic", "viewer").await;

    for user in ["devin", "vic"] {
        for (method, path, body) in [
            (
                Method::POST,
                format!("/v1/organizations/{org}/apps"),
                app("mine", "token"),
            ),
            (
                Method::PATCH,
                format!("/v1/organizations/{org}/apps/{}", id(&catalog)),
                json!({ "name": "Mine" }),
            ),
            (Method::POST, format!("/v1/workspaces/{ws}/apps"), app("mine", "token")),
            (
                Method::PATCH,
                format!("/v1/workspaces/{ws}/apps/{}", id(&own)),
                json!({ "name": "Mine" }),
            ),
        ] {
            let (status, body) = server
                .send(call(user, method.clone(), &path).naming(&org).body(body))
                .await;
            assert_eq!(
                (status, code(&body)),
                (StatusCode::FORBIDDEN, "forbidden"),
                "{user} {method} {path}"
            );
        }
        for path in [
            format!("/v1/organizations/{org}/apps"),
            format!("/v1/organizations/{org}/apps/{}", id(&catalog)),
            format!("/v1/workspaces/{ws}/apps"),
            format!("/v1/workspaces/{ws}/apps/{}", id(&own)),
        ] {
            let (status, _) = server.send(call(user, Method::GET, &path).naming(&org)).await;
            assert_eq!(status, StatusCode::OK, "{user} reads {path}");
        }
    }
    catalog_app(&server, "dana", &org, "dana_tools").await;
    workspace_app(&server, "dana", &org, ws, "dana_own_tools").await;
}

#[tokio::test]
async fn keeps_workspace_apps_in_their_own_workspace() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let sandbox = server.workspace("alice", &org, "acme-sandbox", "sandbox").await;
    let production = server.workspace("alice", &org, "acme", "production").await;
    let (one, two) = (sandbox["id"].as_str().unwrap(), production["id"].as_str().unwrap());
    let own = workspace_app(&server, "alice", &org, one, "echo").await;
    assert_eq!(
        (&own["owner_scope"], &own["owner_workspace_id"]),
        (&json!("workspace"), &json!(one))
    );
    assert_eq!(
        own["configurations"],
        json!([{ "environment": "sandbox", "status": "unconfigured", "version": 0, "updated_at": null }]),
        "only the workspace's own environment"
    );
    let catalog = catalog_app(&server, "alice", &org, "linear").await;

    // Its own workspace finds it; the other workspace, and the catalog, don't.
    let (status, _) = server
        .send(call("alice", Method::GET, format!("/v1/workspaces/{one}/apps/{}", id(&own))).naming(&org))
        .await;
    assert_eq!(status, StatusCode::OK);
    for (method, path) in [
        (Method::GET, format!("/v1/workspaces/{two}/apps/{}", id(&own))),
        (Method::PATCH, format!("/v1/workspaces/{two}/apps/{}", id(&own))),
        (Method::GET, format!("/v1/organizations/{org}/apps/{}", id(&own))),
        (Method::PATCH, format!("/v1/organizations/{org}/apps/{}", id(&own))),
        // Nor is a catalog app a workspace's.
        (Method::GET, format!("/v1/workspaces/{one}/apps/{}", id(&catalog))),
        (Method::PATCH, format!("/v1/workspaces/{one}/apps/{}", id(&catalog))),
        // Nor is anything that isn't an app ID.
        (Method::GET, format!("/v1/workspaces/{one}/apps/{one}")),
        (Method::GET, format!("/v1/organizations/{org}/apps/not-an-id")),
    ] {
        let (status, body) = server
            .send(
                call("alice", method.clone(), &path)
                    .naming(&org)
                    .body(json!({ "name": "Moved" })),
            )
            .await;
        assert_eq!(
            (status, body["error"]["message"].as_str()),
            (StatusCode::NOT_FOUND, Some("App was not found.")),
            "{method} {path}"
        );
    }
    let listed = |body: &Value| -> Vec<String> {
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|app| app["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let (_, in_two) = server
        .send(call("alice", Method::GET, format!("/v1/workspaces/{two}/apps")).naming(&org))
        .await;
    assert!(listed(&in_two).is_empty());
    let (_, in_catalog) = server
        .send(call("alice", Method::GET, format!("/v1/organizations/{org}/apps")))
        .await;
    assert_eq!(listed(&in_catalog), [id(&catalog)]);

    // One key space for the whole organization (decision 0047): a key one
    // workspace holds is taken in every other and in the catalog, and a
    // catalog key in every workspace. The conflict never says where.
    let create_in = |workspace: &str, key: &str| {
        call("alice", Method::POST, format!("/v1/workspaces/{workspace}/apps"))
            .naming(&org)
            .body(app(key, "token"))
    };
    let mut refusals = Vec::new();
    for (workspace, key) in [(one, "echo"), (two, "echo"), (two, "linear")] {
        refusals.push(server.send(create_in(workspace, key)).await);
    }
    refusals.push(
        server
            .send(call("alice", Method::POST, format!("/v1/organizations/{org}/apps")).body(app("echo", "token")))
            .await,
    );
    for (status, body) in &refusals {
        assert_eq!((*status, code(body)), (StatusCode::CONFLICT, "app_key_taken"));
        assert_eq!(body["error"], refusals[0].1["error"], "every conflict reads the same");
    }
    let (_, in_two) = server
        .send(call("alice", Method::GET, format!("/v1/workspaces/{two}/apps")).naming(&org))
        .await;
    assert!(
        in_two["data"].as_array().unwrap().is_empty(),
        "nothing refused was kept"
    );
    // Another organization's keys never count.
    let other = server.organization("riley", "larkspur").await;
    catalog_app(&server, "riley", &other, "echo").await;
}

#[tokio::test]
async fn hides_another_organizations_apps_as_404() {
    let server = Server::new().await;
    let northwind = server.organization("alice", "northwind").await;
    let larkspur = server.organization("riley", "larkspur").await;
    let ws = server.workspace("alice", &northwind, "acme", "sandbox").await;
    let ws = ws["id"].as_str().unwrap();
    let theirs = server.workspace("riley", &larkspur, "harbor", "sandbox").await;
    let theirs = theirs["id"].as_str().unwrap();
    let catalog = catalog_app(&server, "alice", &northwind, "linear").await;
    let own = workspace_app(&server, "alice", &northwind, ws, "echo").await;

    for (method, path, organization) in [
        (Method::GET, format!("/v1/organizations/{northwind}/apps"), &larkspur),
        (Method::POST, format!("/v1/organizations/{northwind}/apps"), &larkspur),
        (
            Method::GET,
            format!("/v1/organizations/{northwind}/apps/{}", id(&catalog)),
            &larkspur,
        ),
        (
            Method::PATCH,
            format!("/v1/organizations/{northwind}/apps/{}", id(&catalog)),
            &larkspur,
        ),
        // Their own organization's route, with Northwind's app ID.
        (
            Method::GET,
            format!("/v1/organizations/{larkspur}/apps/{}", id(&catalog)),
            &larkspur,
        ),
        (
            Method::PATCH,
            format!("/v1/organizations/{larkspur}/apps/{}", id(&catalog)),
            &larkspur,
        ),
        // Northwind's workspace, named under either organization.
        (Method::GET, format!("/v1/workspaces/{ws}/apps"), &larkspur),
        (Method::POST, format!("/v1/workspaces/{ws}/apps"), &larkspur),
        (Method::GET, format!("/v1/workspaces/{ws}/apps"), &northwind),
        (
            Method::GET,
            format!("/v1/workspaces/{ws}/apps/{}", id(&own)),
            &northwind,
        ),
        // Their own workspace, with Northwind's app ID.
        (
            Method::GET,
            format!("/v1/workspaces/{theirs}/apps/{}", id(&own)),
            &larkspur,
        ),
        (
            Method::PATCH,
            format!("/v1/workspaces/{theirs}/apps/{}", id(&own)),
            &larkspur,
        ),
    ] {
        let (status, body) = server
            .send(
                call("riley", method.clone(), &path)
                    .naming(organization)
                    .body(app("stolen", "token")),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{method} {path} as {organization}: {body}"
        );
    }
    // And nothing changed.
    let (_, still) = server
        .send(call(
            "alice",
            Method::GET,
            format!("/v1/organizations/{northwind}/apps"),
        ))
        .await;
    assert_eq!(still["data"].as_array().unwrap().len(), 1);
    assert_eq!(still["data"][0]["name"], "Tools");
    let (_, own_apps) = server
        .send(call("alice", Method::GET, format!("/v1/workspaces/{ws}/apps")).naming(&northwind))
        .await;
    assert_eq!(
        own_apps["data"].as_array().unwrap().len(),
        1,
        "riley's POST added nothing"
    );
    assert_eq!(own_apps["data"][0]["name"], "Tools", "nor did a PATCH rename it");
    assert_eq!(own_apps["data"][0]["updated_at"], own["updated_at"]);
}

#[tokio::test]
async fn lets_a_key_reach_apps_in_its_environment_only() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let sandbox = server.workspace("alice", &org, "acme-sandbox", "sandbox").await;
    let production = server.workspace("alice", &org, "acme", "production").await;
    let (sandbox, production) = (sandbox["id"].as_str().unwrap(), production["id"].as_str().unwrap());
    let in_production = workspace_app(&server, "alice", &org, production, "echo").await;
    let (status, body) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/api-keys"))
                .body(json!({ "name": "Server", "environment": "sandbox" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let key = body["data"]["secret"].as_str().unwrap();

    // The catalog is the organization's: a key of either environment reads it and adds to it.
    let (status, created) = server
        .send(call(key, Method::POST, format!("/v1/organizations/{org}/apps")).body(app("linear", "oauth2")))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(
        created["data"]["configurations"],
        json!([{ "environment": "sandbox", "status": "unconfigured", "version": 0, "updated_at": null }]),
        "a key sees its own environment only"
    );
    // Its own environment's workspaces, yes; the other's, never.
    let (status, _) = server
        .send(call(key, Method::POST, format!("/v1/workspaces/{sandbox}/apps")).body(app("mine", "token")))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    for (method, path) in [
        (Method::GET, format!("/v1/workspaces/{production}/apps")),
        (Method::POST, format!("/v1/workspaces/{production}/apps")),
        (
            Method::GET,
            format!("/v1/workspaces/{production}/apps/{}", id(&in_production)),
        ),
        (
            Method::PATCH,
            format!("/v1/workspaces/{production}/apps/{}", id(&in_production)),
        ),
    ] {
        let (status, _) = server
            .send(call(key, method.clone(), &path).body(app("other", "token")))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
    }

    // A Production key, the other way round.
    let (_, body) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/api-keys"))
                .body(json!({ "name": "Live", "environment": "production" })),
        )
        .await;
    let live = body["data"]["secret"].as_str().unwrap();
    let (status, _) = server
        .send(call(
            live,
            Method::GET,
            format!("/v1/workspaces/{production}/apps/{}", id(&in_production)),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = server
        .send(call(live, Method::GET, format!("/v1/workspaces/{sandbox}/apps")))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The idempotency middleware's workspace scope: one key in two workspaces is
/// two requests, and in the same workspace is one.
#[tokio::test]
async fn keeps_an_idempotency_key_to_its_workspace() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let one = server.workspace("alice", &org, "one", "sandbox").await;
    let two = server.workspace("alice", &org, "two", "sandbox").await;
    // Keys are unique across the organization, so each workspace's app has its own.
    let post = |ws: &str, key: &str| {
        call("alice", Method::POST, format!("/v1/workspaces/{ws}/apps"))
            .naming(&org)
            .idempotent("same-key")
            .body(app(key, "token"))
    };
    let (one, two) = (one["id"].as_str().unwrap(), two["id"].as_str().unwrap());
    let first = server.send(post(one, "echo_one")).await;
    let other = server.send(post(two, "echo_two")).await;
    assert_eq!((first.0, other.0), (StatusCode::CREATED, StatusCode::CREATED));
    assert_ne!(
        id(&first.1["data"]),
        id(&other.1["data"]),
        "each workspace made its own"
    );
    assert_eq!(other.1["data"]["owner_workspace_id"], two);
    let again = server.send(post(one, "echo_one")).await;
    assert_eq!(again, first, "a replay in the first workspace");
}

/// What a template is checked for beyond the shared suite: each shape error
/// on its own field, and a workspace app's configuration keeping its scopes.
#[tokio::test]
async fn checks_a_new_template_field_by_field() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let ws = server.workspace("alice", &org, "acme-sandbox", "sandbox").await;
    let ws = ws["id"].as_str().unwrap();
    let oauth = workspace_app(&server, "alice", &org, ws, "crm").await;
    let path = format!("/v1/workspaces/{ws}/apps/{}", id(&oauth));
    let patch = |auth: Value| {
        call("alice", Method::PATCH, &path)
            .naming(&org)
            .body(json!({ "auth": auth }))
    };

    let (status, body) = server.send(patch(json!({ "type": "basic" }))).await;
    assert_eq!(
        (status, &body["error"]["fields"]),
        (
            StatusCode::BAD_REQUEST,
            &json!({ "auth.type": "Choose oauth2, mcp_oauth or token." })
        )
    );

    // A token app (the helper makes token apps): every field of the template is checked.
    let (status, body) = server
        .send(patch(json!({
            "type": "token",
            "inputs": [{ "key": "Bad Key", "label": "Token", "secret": true }],
            "placement": { "location": "query", "name": "Authorization", "prefix": "Bearer\n" },
            "verify_url": 7,
            "instructions": "x".repeat(1001),
        })))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let fields: Vec<&str> = body["error"]["fields"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        fields,
        [
            "auth.docs_url",
            "auth.inputs",
            "auth.instructions",
            "auth.placement.location",
            "auth.placement.prefix",
            "auth.verify_url",
        ]
    );
    let token = |verify_url: &str| {
        json!({
            "type": "token",
            "inputs": [{ "key": "api_key", "label": "Key", "secret": true, "help": null, "placeholder": null, "pattern": null }],
            "placement": { "location": "header", "name": "X-Api-Key", "prefix": "" },
            "verify_url": verify_url,
            "instructions": "",
            "docs_url": null,
        })
    };
    for (url, expected) in [
        ("https://user:pass@api.example/me", "destination_not_allowed"),
        ("https://localhost/me", "destination_not_allowed"),
        ("http://api.example/me", "insecure_url"),
    ] {
        let (_, body) = server.send(patch(token(url))).await;
        assert_eq!(
            (code(&body), &body["error"]["fields"]["auth.verify_url"].is_string()),
            (expected, &true),
            "{url}"
        );
    }
    let (status, body) = server.send(patch(token("https://api.example/me"))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["version"], 2);

    // A workspace OAuth app keeps the scopes its configuration requests.
    let (_, created) = server
        .send(
            call("alice", Method::POST, format!("/v1/workspaces/{ws}/apps"))
                .naming(&org)
                .body(app("tickets", "oauth2")),
        )
        .await;
    let tickets = format!("/v1/workspaces/{ws}/apps/{}", id(&created["data"]));
    let template = |scopes: Value| json!({ "type": "oauth2", "authorize_url": null, "token_url": null, "identity_url": null, "available_scopes": scopes, "pkce": true });
    let (status, _) = server
        .send(
            call("alice", Method::PATCH, &tickets)
                .naming(&org)
                .body(json!({ "auth": template(json!([{ "name": "read", "description": "" }])) })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    server
        .send(
            call("alice", Method::PUT, format!("{tickets}/configuration"))
                .naming(&org)
                .body(json!({ "oauth_scopes": ["read"] })),
        )
        .await;
    let (status, body) = server
        .send(
            call("alice", Method::PATCH, &tickets)
                .naming(&org)
                .body(json!({ "auth": template(json!([])) })),
        )
        .await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "scope_in_use"));
    assert_eq!(
        body["error"]["fields"]["auth.available_scopes"],
        "read is still requested by the Sandbox configuration."
    );
    let duplicate =
        template(json!([{ "name": "read", "description": "" }, { "name": "read", "description": "again" }]));
    let (status, body) = server
        .send(
            call("alice", Method::PATCH, &tickets)
                .naming(&org)
                .body(json!({ "auth": duplicate })),
        )
        .await;
    assert_eq!(
        (status, body["error"]["fields"]["auth.available_scopes"].is_string()),
        (StatusCode::BAD_REQUEST, true)
    );
}

/// A catalog app's template is shared by both environments, so no API key,
/// acting in one of them, may change it, and its refusal names nothing of
/// the other environment. A workspace app lives in one environment: its
/// key may.
#[tokio::test]
async fn keeps_api_keys_off_a_shared_template() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let sandbox = server.workspace("alice", &org, "acme-sandbox", "sandbox").await;
    let sandbox = sandbox["id"].as_str().unwrap();
    let (_, created) = server
        .send(call("alice", Method::POST, format!("/v1/organizations/{org}/apps")).body(app("tracker", "oauth2")))
        .await;
    let path = format!("/v1/organizations/{org}/apps/{}", id(&created["data"]));
    let template = |token_url: &str, scopes: Value| json!({ "type": "oauth2", "authorize_url": null, "token_url": token_url, "issuer": "https://AUTH.provider.example/Tenant/", "identity_url": null, "available_scopes": scopes, "pkce": true });
    let owned = template(
        "https://auth.tracker.example/token",
        json!([{ "name": "secret:admin", "description": "" }]),
    );
    let (status, _) = server
        .send(call("alice", Method::PATCH, &path).body(json!({ "auth": owned })))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = server
        .send(
            call(
                "alice",
                Method::PUT,
                format!("{path}/environments/production/configuration"),
            )
            .body(json!({ "oauth_scopes": ["secret:admin"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    for environment in ["sandbox", "production"] {
        let (_, key) = server
            .send(
                call("alice", Method::POST, format!("/v1/organizations/{org}/api-keys"))
                    .body(json!({ "name": "Server", "environment": environment })),
            )
            .await;
        let key = key["data"]["secret"].as_str().unwrap();
        for auth in [
            template(
                "https://attacker.example/t",
                json!([{ "name": "secret:admin", "description": "" }]),
            ),
            template("https://auth.tracker.example/token", json!([])),
        ] {
            let (status, body) = server
                .send(call(key, Method::PATCH, &path).body(json!({ "auth": auth })))
                .await;
            assert_eq!(
                (status, code(&body)),
                (StatusCode::FORBIDDEN, "console_only"),
                "{environment}"
            );
            assert!(
                !body.to_string().contains("secret:admin"),
                "nothing of a configuration is named"
            );
        }
        // Renaming stays open to keys.
        let (status, _) = server
            .send(call(key, Method::PATCH, &path).body(json!({ "name": format!("Tracker {environment}") })))
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (_, after) = server.send(call("alice", Method::GET, &path)).await;
    assert_eq!(
        after["data"]["auth"]["token_url"], "https://auth.tracker.example/token",
        "unchanged"
    );

    // A workspace app's key may change its workspace app's template.
    let (_, key) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/api-keys"))
                .body(json!({ "name": "Server", "environment": "sandbox" })),
        )
        .await;
    let key = key["data"]["secret"].as_str().unwrap();
    let (_, own) = server
        .send(call(key, Method::POST, format!("/v1/workspaces/{sandbox}/apps")).body(app("crm", "oauth2")))
        .await;
    let (status, body) = server
        .send(
            call(
                key,
                Method::PATCH,
                format!("/v1/workspaces/{sandbox}/apps/{}", id(&own["data"])),
            )
            .body(json!({ "auth": template("https://auth.crm.example/token", json!([])) })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["auth"]["issuer"], "https://AUTH.provider.example/Tenant/");
}

/// Patterns Connect checks in the browser must mean the same here; trailing
/// dots don't hide a local name; and a dropped scope (409) is reported before
/// a bad URL (422).
#[tokio::test]
async fn checks_patterns_hosts_and_the_order_of_refusals() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let ws = server.workspace("alice", &org, "acme-sandbox", "sandbox").await;
    let ws = ws["id"].as_str().unwrap();
    let token = workspace_app(&server, "alice", &org, ws, "tools").await;
    let path = format!("/v1/workspaces/{ws}/apps/{}", id(&token));
    let with_pattern = |pattern: &str| {
        json!({ "auth": {
            "type": "token",
            "inputs": [{ "key": "token", "label": "Token", "secret": true, "help": null, "placeholder": null, "pattern": pattern }],
            "placement": { "location": "header", "name": "Authorization", "prefix": "Bearer " },
            "verify_url": null, "instructions": "", "docs_url": null,
        }})
    };
    for pattern in ["(", "(?=tk)tk", "(?i)tk", "(?<name>tk)", r"(tk)\1", r"(?P<n>tk)"] {
        let (status, body) = server
            .send(
                call("alice", Method::PATCH, &path)
                    .naming(&org)
                    .body(with_pattern(pattern)),
            )
            .await;
        assert_eq!(
            (status, &body["error"]["fields"]["auth.inputs"]),
            (
                StatusCode::BAD_REQUEST,
                &json!(
                    "Use a simple regular expression: characters, [classes] with ranges, (groups), \\d \\w \\s, and one quantifier (* + ? {m,n}) each."
                )
            ),
            "{pattern}"
        );
    }
    for pattern in [r"^tk_[a-z0-9]{8,}$", r"^(?:sk|rk)_(test|live)_\w+$", r"\d{4}"] {
        let (status, body) = server
            .send(
                call("alice", Method::PATCH, &path)
                    .naming(&org)
                    .body(with_pattern(pattern)),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{pattern}: {body}");
    }
    let mut dotted = with_pattern("^x$");
    dotted["auth"]["verify_url"] = json!("https://localhost../me");
    let (status, body) = server
        .send(call("alice", Method::PATCH, &path).naming(&org).body(dotted))
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "destination_not_allowed")
    );

    let (_, created) = server
        .send(
            call("alice", Method::POST, format!("/v1/workspaces/{ws}/apps"))
                .naming(&org)
                .body(app("tickets", "oauth2")),
        )
        .await;
    let tickets = format!("/v1/workspaces/{ws}/apps/{}", id(&created["data"]));
    let template = |token_url: &str, scopes: Value| json!({ "auth": { "type": "oauth2", "authorize_url": null, "token_url": token_url, "identity_url": null, "available_scopes": scopes, "pkce": true } });
    server
        .send(call("alice", Method::PATCH, &tickets).naming(&org).body(template(
            "https://auth.example/t",
            json!([{ "name": "read", "description": "" }]),
        )))
        .await;
    server
        .send(
            call("alice", Method::PUT, format!("{tickets}/configuration"))
                .naming(&org)
                .body(json!({ "oauth_scopes": ["read"] })),
        )
        .await;
    let (status, body) = server
        .send(
            call("alice", Method::PATCH, &tickets)
                .naming(&org)
                .body(template("http://insecure.example/t", json!([]))),
        )
        .await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "scope_in_use"));
}
