//! Apps end to end (decision 0031): the catalog and each workspace's own apps,
//! never reachable through each other's routes, another workspace's or another
//! organization's, and managed by admins only.

#![allow(clippy::unwrap_used)]

mod common;

use axum::http::{Method, StatusCode};
use common::{Server, call, code};
use serde_json::{Value, json};

fn app(slug: &str, auth_type: &str) -> Value {
    json!({ "kind": "mcp", "name": "Tools", "slug": slug, "description": "  Internal tools.  ", "auth_type": auth_type })
}

/// Creates a catalog app as `user`; its body.
async fn catalog_app(server: &Server, user: &str, org: &str, slug: &str) -> Value {
    let (status, body) = server
        .send(call(user, Method::POST, format!("/v1/organizations/{org}/apps")).body(app(slug, "token")))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["data"].clone()
}

/// Creates a workspace app as `user`; its body.
async fn workspace_app(server: &Server, user: &str, org: &str, ws: &str, slug: &str) -> Value {
    let (status, body) = server
        .send(
            call(user, Method::POST, format!("/v1/workspaces/{ws}/apps"))
                .naming(org)
                .body(app(slug, "token")),
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
        (&created["kind"], &created["slug"], &created["description"]),
        (&json!("mcp"), &json!("linear"), &json!("Internal tools.")),
        "the description is trimmed"
    );
    assert_eq!(created["icon"], json!({ "glyph": "T", "color": "#15803D" }));
    assert_eq!(
        created["auth"],
        json!({
            "type": "oauth2", "authorize_url": null, "token_url": null,
            "available_scopes": [], "pkce": true, "identity_url": null,
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
            &updated["data"]["slug"]
        ),
        (&json!("Linear"), &json!(""), &json!("linear"))
    );
    let (_, untouched) = server.send(call("alice", Method::PATCH, &path).body(json!({}))).await;
    assert_eq!(untouched["data"]["name"], "Linear", "an empty update changes nothing");

    catalog_app(&server, "alice", &org, "acme-tools").await;
    let (status, list) = server
        .send(call("alice", Method::GET, format!("/v1/organizations/{org}/apps")))
        .await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|app| app["slug"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["linear", "acme-tools"], "by name: Linear, then Tools");

    let (status, taken) = server
        .send(call("alice", Method::POST, format!("/v1/organizations/{org}/apps")).body(app("linear", "token")))
        .await;
    assert_eq!((status, code(&taken)), (StatusCode::CONFLICT, "slug_taken"));
    assert_eq!(taken["error"]["fields"]["slug"], "Already used by another app here.");
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
    assert_eq!(named, ["auth_type", "description", "kind", "name", "slug"]);
    assert_eq!(fields["auth_type"], "Choose OAuth or token.");

    let mut wrong = app("ok", "basic");
    wrong["description"] = json!("x".repeat(281));
    wrong["slug"] = json!("Not A Slug");
    let (_, body) = server.send(call("alice", Method::POST, &path).body(wrong)).await;
    assert_eq!(
        body["error"]["fields"],
        json!({
            "auth_type": "Choose OAuth or token.",
            "description": "Use 280 characters or fewer.",
            "slug": "Use 1–40 lowercase letters, numbers and hyphens.",
        })
    );

    // A kind that isn't `mcp` is refused, never coerced, once the rest is valid.
    let mut other = app("other", "token");
    other["kind"] = json!("openapi");
    let (status, body) = server.send(call("alice", Method::POST, &path).body(other)).await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, "unsupported_app_kind"));
    assert_eq!(body["error"]["fields"]["kind"], "Only \"mcp\" is supported.");

    let mut token = app("token-app", "token");
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
    catalog_app(&server, "dana", &org, "dana-tools").await;
    workspace_app(&server, "dana", &org, ws, "dana-tools").await;
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

    // Slugs are unique per owner: the catalog and the other workspace may reuse it.
    workspace_app(&server, "alice", &org, two, "echo").await;
    catalog_app(&server, "alice", &org, "echo").await;
    let (status, body) = server
        .send(
            call("alice", Method::POST, format!("/v1/workspaces/{one}/apps"))
                .naming(&org)
                .body(app("echo", "token")),
        )
        .await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "slug_taken"));
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
    assert_eq!(created["data"]["configurations"].as_array().unwrap().len(), 2);
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
    let post = |ws: &str| {
        call("alice", Method::POST, format!("/v1/workspaces/{ws}/apps"))
            .naming(&org)
            .idempotent("same-key")
            .body(app("echo", "token"))
    };
    let (one, two) = (one["id"].as_str().unwrap(), two["id"].as_str().unwrap());
    let first = server.send(post(one)).await;
    let other = server.send(post(two)).await;
    assert_eq!((first.0, other.0), (StatusCode::CREATED, StatusCode::CREATED));
    assert_ne!(
        id(&first.1["data"]),
        id(&other.1["data"]),
        "each workspace made its own"
    );
    assert_eq!(other.1["data"]["owner_workspace_id"], two);
    let again = server.send(post(one)).await;
    assert_eq!(again, first, "a replay in the first workspace");
}
