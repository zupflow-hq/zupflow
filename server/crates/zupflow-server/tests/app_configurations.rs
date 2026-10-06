//! Client configurations end to end (decisions 0031 and 0033): written field
//! by field, client secrets sealed to their owner and never shown, endpoints
//! checked, environments and owners kept apart, and only what is ready in a
//! workspace's own environment offered to it.

#![allow(clippy::unwrap_used)]

mod common;

use axum::http::{Method, StatusCode};
use common::{Server, call, code};
use serde_json::{Value, json};
use zupflow_basic::local_keys::LocalKeyService;
use zupflow_core::environment::Environment;
use zupflow_core::ids::{AppId, OrganizationId, WorkspaceId};
use zupflow_core::keys::{self, EncryptionContext, KeyError, Sealed, WrappedKey};

const SECRET: &str = "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k";

fn app(slug: &str, auth_type: &str) -> Value {
    json!({ "kind": "mcp", "name": slug.to_uppercase(), "slug": slug, "description": "", "auth_type": auth_type })
}

async fn created(server: &Server, user: &str, path: String, org: &str, body: Value) -> Value {
    let (status, body) = server.send(call(user, Method::POST, path).naming(org).body(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["data"].clone()
}

fn id(value: &Value) -> &str {
    value["id"].as_str().unwrap()
}

/// The sealed secret as stored, read straight from the database.
async fn stored_secret(server: &Server, app_id: &str, environment: &str) -> (Sealed, i64) {
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", server.directory.join("zupflow.sqlite").display()))
        .await
        .unwrap();
    let app = AppId::parse(app_id).unwrap();
    let row: (String, Vec<u8>, Vec<u8>, Vec<u8>, i64) = sqlx::query_as(
        "SELECT secret_key_id, secret_wrapped_key, secret_nonce, secret_ciphertext, secret_version \
         FROM app_configurations WHERE app_id = ? AND environment = ?",
    )
    .bind(app.uuid())
    .bind(environment)
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    let sealed = Sealed {
        wrapped_key: WrappedKey {
            key_id: row.0,
            bytes: row.1,
        },
        nonce: row.2.try_into().unwrap(),
        ciphertext: row.3,
    };
    (sealed, row.4)
}

#[tokio::test]
async fn configures_a_catalog_app_and_seals_its_secret() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let linear = created(
        &server,
        "alice",
        format!("/v1/organizations/{org}/apps"),
        &org,
        app("linear", "oauth2"),
    )
    .await;
    let path = format!(
        "/v1/organizations/{org}/apps/{}/environments/sandbox/configuration",
        id(&linear)
    );

    let (status, empty) = server.send(call("alice", Method::GET, &path)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        empty["data"],
        json!({
            "app_id": id(&linear), "environment": "sandbox", "status": "unconfigured", "version": 0,
            "mcp": { "server_url": null, "transport": "streamable_http" },
            "oauth": {
                "client_id": null,
                "client_secret": { "set": false, "last4": null, "updated_at": null },
                "redirect_uri": "http://localhost:8080/connect/oauth/callback",
                "scopes": [],
            },
            "missing": ["mcp_server_url", "oauth_client_id", "oauth_client_secret"],
            "updated_at": null, "updated_by": null,
        })
    );

    let (status, first) = server
        .send(call("alice", Method::PUT, &path).body(json!({ "mcp_server_url": " https://mcp.linear.example/mcp " })))
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let first = &first["data"];
    assert_eq!((&first["status"], &first["version"]), (&json!("incomplete"), &json!(1)));
    assert_eq!(first["mcp"]["server_url"], "https://mcp.linear.example/mcp");
    assert_eq!(first["updated_by"]["email"], "alice@people.example");

    let (status, ready) = server
        .send(
            call("alice", Method::PUT, &path)
                .body(json!({ "oauth_client_id": "client-1", "oauth_client_secret": SECRET })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{ready}");
    assert!(!ready.to_string().contains(SECRET), "the secret is never in a response");
    let ready = &ready["data"];
    assert_eq!(
        (&ready["status"], &ready["version"], &ready["missing"]),
        (&json!("ready"), &json!(2), &json!([]))
    );
    assert_eq!(ready["mcp"]["server_url"], "https://mcp.linear.example/mcp", "kept");
    assert_eq!(ready["oauth"]["client_secret"]["set"], true);
    assert_eq!(ready["oauth"]["client_secret"]["last4"], "9f3k");
    let (_, again) = server.send(call("alice", Method::GET, &path)).await;
    assert_eq!(&again["data"], ready);
    assert!(!again.to_string().contains(SECRET));

    // Stored sealed: not the secret, and it opens only under its own context.
    let (sealed, version) = stored_secret(&server, id(&linear), "sandbox").await;
    assert_eq!(version, 1);
    assert!(
        !sealed
            .ciphertext
            .windows(SECRET.len())
            .any(|window| window == SECRET.as_bytes())
    );
    let keys = LocalKeyService::from_server_secret(&[7; 32]);
    let (org_id, app_id) = (OrganizationId::parse(&org).unwrap(), AppId::parse(id(&linear)).unwrap());
    let context = |organization, environment, version| {
        EncryptionContext::oauth_client_secret(organization, None, environment, app_id, version)
    };
    let opened = keys::open(&keys, &context(org_id, Environment::Sandbox, 1), &sealed)
        .await
        .unwrap();
    assert_eq!(opened.expose(), SECRET.as_bytes());
    for (organization, environment, version) in [
        (org_id, Environment::Production, 1),
        (org_id, Environment::Sandbox, 2),
        (OrganizationId::generate(), Environment::Sandbox, 1),
    ] {
        assert_eq!(
            keys::open(&keys, &context(organization, environment, version), &sealed)
                .await
                .unwrap_err(),
            KeyError::Refused
        );
    }

    // A new secret is the next version; leaving it out keeps it; null clears the server.
    let (_, rotated) = server
        .send(call("alice", Method::PUT, &path).body(json!({ "oauth_client_secret": "another-secret-abcd" })))
        .await;
    assert_eq!(rotated["data"]["oauth"]["client_secret"]["last4"], "abcd");
    let (sealed, version) = stored_secret(&server, id(&linear), "sandbox").await;
    assert_eq!(version, 2);
    let opened = keys::open(&keys, &context(org_id, Environment::Sandbox, 2), &sealed)
        .await
        .unwrap();
    assert_eq!(opened.expose(), b"another-secret-abcd");
    let (_, cleared) = server
        .send(call("alice", Method::PUT, &path).body(json!({ "mcp_server_url": null })))
        .await;
    assert_eq!(
        (
            &cleared["data"]["status"],
            &cleared["data"]["missing"],
            &cleared["data"]["oauth"]["client_secret"]["last4"]
        ),
        (&json!("incomplete"), &json!(["mcp_server_url"]), &json!("abcd"))
    );

    // Production is its own.
    let production = path.replace("/sandbox/", "/production/");
    let (_, other) = server.send(call("alice", Method::GET, &production)).await;
    assert_eq!(
        (&other["data"]["version"], &other["data"]["status"]),
        (&json!(0), &json!("unconfigured"))
    );

    // The app shows both environments' summaries.
    let (_, shown) = server
        .send(call(
            "alice",
            Method::GET,
            format!("/v1/organizations/{org}/apps/{}", id(&linear)),
        ))
        .await;
    let summaries = &shown["data"]["configurations"];
    assert_eq!(
        (
            &summaries[0]["environment"],
            &summaries[0]["status"],
            &summaries[0]["version"]
        ),
        (&json!("sandbox"), &json!("incomplete"), &json!(4))
    );
    assert_eq!(summaries[1]["status"], "unconfigured");
}

#[tokio::test]
async fn checks_what_a_configuration_is_set_to() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let oauth = created(
        &server,
        "alice",
        format!("/v1/organizations/{org}/apps"),
        &org,
        app("linear", "oauth2"),
    )
    .await;
    let token = created(
        &server,
        "alice",
        format!("/v1/organizations/{org}/apps"),
        &org,
        app("tools", "token"),
    )
    .await;
    let put = |app: &Value, body: Value| {
        call(
            "alice",
            Method::PUT,
            format!(
                "/v1/organizations/{org}/apps/{}/environments/sandbox/configuration",
                id(app)
            ),
        )
        .body(body)
    };

    let (status, body) = server
        .send(put(
            &oauth,
            json!({ "oauth_client_secret": "short", "oauth_client_id": 7, "oauth_scopes": "read", "mcp_server_url": "x".repeat(501) }),
        ))
        .await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, "validation_failed"));
    assert_eq!(
        body["error"]["fields"],
        json!({
            "mcp_server_url": "Use 500 characters or fewer.",
            "oauth_client_id": "Must be a string or null.",
            "oauth_client_secret": "Paste the full client secret.",
            "oauth_scopes": "Must be a list of scopes.",
        })
    );

    for (url, status, expected) in [
        ("not a url", StatusCode::BAD_REQUEST, "validation_failed"),
        (
            "http://mcp.example/mcp",
            StatusCode::UNPROCESSABLE_ENTITY,
            "insecure_url",
        ),
        (
            "https://169.254.169.254/mcp",
            StatusCode::UNPROCESSABLE_ENTITY,
            "destination_not_allowed",
        ),
        (
            "https://127.1/mcp",
            StatusCode::UNPROCESSABLE_ENTITY,
            "destination_not_allowed",
        ),
        (
            "https://2130706433/mcp",
            StatusCode::UNPROCESSABLE_ENTITY,
            "destination_not_allowed",
        ),
        (
            "https://[::ffff:10.0.0.1]/mcp",
            StatusCode::UNPROCESSABLE_ENTITY,
            "destination_not_allowed",
        ),
        (
            "https://localhost/mcp",
            StatusCode::UNPROCESSABLE_ENTITY,
            "destination_not_allowed",
        ),
        (
            "https://api.localhost/mcp",
            StatusCode::UNPROCESSABLE_ENTITY,
            "destination_not_allowed",
        ),
        (
            "https://metadata.google.internal/mcp",
            StatusCode::UNPROCESSABLE_ENTITY,
            "destination_not_allowed",
        ),
        (
            "https://user:pass@mcp.example/mcp",
            StatusCode::UNPROCESSABLE_ENTITY,
            "destination_not_allowed",
        ),
    ] {
        let (got, body) = server.send(put(&oauth, json!({ "mcp_server_url": url }))).await;
        assert_eq!((got, code(&body)), (status, expected), "{url}");
        assert!(body["error"]["fields"]["mcp_server_url"].is_string(), "{url}");
    }

    let (status, body) = server.send(put(&oauth, json!({ "oauth_scopes": ["read"] }))).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "unknown_scope")
    );
    assert_eq!(
        body["error"]["fields"]["oauth_scopes"],
        "read is not offered by this app."
    );

    for body in [
        json!({ "oauth_client_id": "client" }),
        json!({ "oauth_client_secret": SECRET }),
    ] {
        let (status, body) = server.send(put(&token, body)).await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::UNPROCESSABLE_ENTITY, "oauth_not_supported")
        );
    }
    // A token app is ready with a server alone, and has no OAuth settings.
    let (status, body) = server
        .send(put(
            &token,
            json!({ "mcp_server_url": "https://tools.example/mcp", "oauth_client_id": "" }),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        (
            &body["data"]["status"],
            &body["data"]["oauth"],
            &body["data"]["missing"]
        ),
        (&json!("ready"), &Value::Null, &json!([]))
    );

    // Nothing refused was written.
    let (_, untouched) = server
        .send(call(
            "alice",
            Method::GET,
            format!(
                "/v1/organizations/{org}/apps/{}/environments/sandbox/configuration",
                id(&oauth)
            ),
        ))
        .await;
    assert_eq!(untouched["data"]["version"], 0);
}

#[tokio::test]
async fn keeps_configurations_to_their_owner_and_environment() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let other = server.organization("riley", "larkspur").await;
    let sandbox = server.workspace("alice", &org, "acme-sandbox", "sandbox").await;
    let production = server.workspace("alice", &org, "acme", "production").await;
    let (sandbox, production) = (id(&sandbox).to_owned(), id(&production).to_owned());
    let catalog = created(
        &server,
        "alice",
        format!("/v1/organizations/{org}/apps"),
        &org,
        app("linear", "oauth2"),
    )
    .await;
    let own = created(
        &server,
        "alice",
        format!("/v1/workspaces/{sandbox}/apps"),
        &org,
        app("echo", "token"),
    )
    .await;
    server.member("alice", &org, "devin", "developer").await;

    let catalog_path = |env: &str| {
        format!(
            "/v1/organizations/{org}/apps/{}/environments/{env}/configuration",
            id(&catalog)
        )
    };
    let own_path = |ws: &str, app: &Value| format!("/v1/workspaces/{ws}/apps/{}/configuration", id(app));

    // A workspace app's is in its workspace's environment, and refuses another.
    let (status, body) = server
        .send(
            call("alice", Method::PUT, own_path(&sandbox, &own))
                .naming(&org)
                .body(json!({ "mcp_server_url": "https://echo.example/mcp" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        (&body["data"]["environment"], &body["data"]["status"]),
        (&json!("sandbox"), &json!("ready"))
    );
    let (status, body) = server
        .send(
            call("alice", Method::PUT, own_path(&sandbox, &own))
                .naming(&org)
                .body(json!({ "environment": "production", "mcp_server_url": "https://elsewhere.example/mcp" })),
        )
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "environment_mismatch")
    );

    // Never through another owner's route.
    for (user, method, path, organization) in [
        ("alice", Method::GET, own_path(&production, &own), &org),
        ("alice", Method::PUT, own_path(&production, &own), &org),
        ("alice", Method::GET, own_path(&sandbox, &catalog), &org),
        (
            "alice",
            Method::GET,
            format!(
                "/v1/organizations/{org}/apps/{}/environments/sandbox/configuration",
                id(&own)
            ),
            &org,
        ),
        ("riley", Method::GET, catalog_path("sandbox"), &other),
        ("riley", Method::PUT, catalog_path("sandbox"), &other),
        (
            "riley",
            Method::GET,
            format!(
                "/v1/organizations/{other}/apps/{}/environments/sandbox/configuration",
                id(&catalog)
            ),
            &other,
        ),
        ("riley", Method::GET, own_path(&sandbox, &own), &org),
        (
            "riley",
            Method::GET,
            format!("/v1/workspaces/{sandbox}/available-apps"),
            &org,
        ),
        ("alice", Method::GET, catalog_path("staging"), &org),
    ] {
        let (status, body) = server
            .send(
                call(user, method.clone(), &path)
                    .naming(organization)
                    .body(json!({ "mcp_server_url": "https://stolen.example/mcp" })),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{user} {method} {path}: {body}");
    }
    let (_, body) = server.send(call("alice", Method::GET, catalog_path("staging"))).await;
    assert_eq!(body["error"]["message"], "Environment was not found.");

    // Developers read, never write.
    let (status, _) = server.send(call("devin", Method::GET, catalog_path("sandbox"))).await;
    assert_eq!(status, StatusCode::OK);
    for (path, organization) in [(catalog_path("sandbox"), &org), (own_path(&sandbox, &own), &org)] {
        let (status, _) = server
            .send(
                call("devin", Method::PUT, &path)
                    .naming(organization)
                    .body(json!({ "mcp_server_url": "https://mine.example/mcp" })),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
    }

    // A key acts in its own environment only.
    let (_, key) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/api-keys"))
                .body(json!({ "name": "Server", "environment": "sandbox" })),
        )
        .await;
    let key = key["data"]["secret"].as_str().unwrap();
    let (status, body) = server
        .send(
            call(key, Method::PUT, catalog_path("sandbox"))
                .body(json!({ "mcp_server_url": "https://mcp.linear.example/mcp" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["updated_by"], Value::Null, "a key writes as nobody");
    for method in [Method::GET, Method::PUT] {
        let (status, body) = server
            .send(call(key, method.clone(), catalog_path("production")).body(json!({})))
            .await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::UNPROCESSABLE_ENTITY, "environment_mismatch"),
            "{method}"
        );
    }

    // Nothing reached through the wrong routes was written.
    let (_, own_now) = server
        .send(call("alice", Method::GET, own_path(&sandbox, &own)).naming(&org))
        .await;
    assert_eq!(own_now["data"]["mcp"]["server_url"], "https://echo.example/mcp");
    assert_eq!(own_now["data"]["version"], 1);
    let (_, catalog_now) = server.send(call("alice", Method::GET, catalog_path("sandbox"))).await;
    assert_eq!(catalog_now["data"]["version"], 1, "the key's write only");
}

#[tokio::test]
async fn offers_a_workspace_only_what_is_ready_in_its_environment() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let sandbox = server.workspace("alice", &org, "acme-sandbox", "sandbox").await;
    let other_sandbox = server.workspace("alice", &org, "qa", "sandbox").await;
    let production = server.workspace("alice", &org, "acme", "production").await;
    let (sandbox, other_sandbox, production) = (
        id(&sandbox).to_owned(),
        id(&other_sandbox).to_owned(),
        id(&production).to_owned(),
    );
    let tools = created(
        &server,
        "alice",
        format!("/v1/organizations/{org}/apps"),
        &org,
        app("tools", "token"),
    )
    .await;
    let linear = created(
        &server,
        "alice",
        format!("/v1/organizations/{org}/apps"),
        &org,
        app("linear", "oauth2"),
    )
    .await;
    let own = created(
        &server,
        "alice",
        format!("/v1/workspaces/{sandbox}/apps"),
        &org,
        app("echo", "token"),
    )
    .await;
    let configure = |path: String, body: Value| call("alice", Method::PUT, path).naming(&org).body(body);
    // `tools` ready in Sandbox only; `linear` incomplete in Sandbox; `echo` ready in its workspace.
    for (path, body) in [
        (
            format!(
                "/v1/organizations/{org}/apps/{}/environments/sandbox/configuration",
                id(&tools)
            ),
            json!({ "mcp_server_url": "https://tools.example/mcp" }),
        ),
        (
            format!(
                "/v1/organizations/{org}/apps/{}/environments/sandbox/configuration",
                id(&linear)
            ),
            json!({ "mcp_server_url": "https://mcp.linear.example/mcp", "oauth_client_id": "client" }),
        ),
        (
            format!("/v1/workspaces/{sandbox}/apps/{}/configuration", id(&own)),
            json!({ "mcp_server_url": "https://echo.example/mcp" }),
        ),
    ] {
        let (status, body) = server.send(configure(path, body)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let available = |ws: String| {
        let server = &server;
        let org = org.clone();
        async move {
            let (status, body) = server
                .send(call("alice", Method::GET, format!("/v1/workspaces/{ws}/available-apps")).naming(&org))
                .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            body["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|available| available["app"]["slug"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(available(sandbox.clone()).await, ["echo", "tools"]);
    assert_eq!(
        available(other_sandbox).await,
        ["tools"],
        "another workspace's app is never offered"
    );
    assert!(
        available(production).await.is_empty(),
        "no fallback to another environment"
    );

    let (_, body) = server
        .send(call("alice", Method::GET, format!("/v1/workspaces/{sandbox}/available-apps")).naming(&org))
        .await;
    let echo = &body["data"][0];
    assert_eq!(
        (&echo["configuration"]["environment"], &echo["configuration"]["status"]),
        (&json!("sandbox"), &json!("ready"))
    );
    assert_eq!(echo["app"]["configurations"].as_array().unwrap().len(), 1);
}

/// `(has_app, has_ready_configuration, has_api_key)` as the organization shows them.
async fn setup(server: &Server, user: &str, org: &str) -> (bool, bool, bool) {
    let (_, body) = server
        .send(call(user, Method::GET, format!("/v1/organizations/{org}")))
        .await;
    let setup = &body["data"]["setup"];
    (
        setup["has_app"].as_bool().unwrap(),
        setup["has_ready_configuration"].as_bool().unwrap(),
        setup["has_api_key"].as_bool().unwrap(),
    )
}

async fn sandbox_key(server: &Server, user: &str, org: &str) -> String {
    let (_, key) = server
        .send(
            call(user, Method::POST, format!("/v1/organizations/{org}/api-keys"))
                .body(json!({ "name": "Server", "environment": "sandbox" })),
        )
        .await;
    key["data"]["key"]["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn tells_each_organization_how_far_its_own_setup_is() {
    let server = Server::new().await;
    let northwind = server.organization("alice", "northwind").await;
    let acme = server.organization("alice", "acme-co").await;
    let larkspur = server.organization("riley", "larkspur").await;

    // Larkspur has everything, through both kinds of app.
    let harbor = server.workspace("riley", &larkspur, "harbor", "sandbox").await;
    let harbor = id(&harbor).to_owned();
    let catalog = created(
        &server,
        "riley",
        format!("/v1/organizations/{larkspur}/apps"),
        &larkspur,
        app("tools", "token"),
    )
    .await;
    let own = created(
        &server,
        "riley",
        format!("/v1/workspaces/{harbor}/apps"),
        &larkspur,
        app("echo", "token"),
    )
    .await;
    for path in [
        format!(
            "/v1/organizations/{larkspur}/apps/{}/environments/sandbox/configuration",
            id(&catalog)
        ),
        format!("/v1/workspaces/{harbor}/apps/{}/configuration", id(&own)),
    ] {
        let (status, _) = server
            .send(
                call("riley", Method::PUT, path)
                    .naming(&larkspur)
                    .body(json!({ "mcp_server_url": "https://tools.example/mcp" })),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    sandbox_key(&server, "riley", &larkspur).await;
    assert_eq!(setup(&server, "riley", &larkspur).await, (true, true, true));
    assert_eq!(
        setup(&server, "alice", &northwind).await,
        (false, false, false),
        "none of Larkspur's counts here"
    );
    assert_eq!(setup(&server, "alice", &acme).await, (false, false, false));

    // Northwind: a workspace app alone has an app, then a ready one.
    let ws = server.workspace("alice", &northwind, "acme-sandbox", "sandbox").await;
    let ws = id(&ws).to_owned();
    let echo = created(
        &server,
        "alice",
        format!("/v1/workspaces/{ws}/apps"),
        &northwind,
        app("echo", "token"),
    )
    .await;
    assert_eq!(setup(&server, "alice", &northwind).await, (true, false, false));
    server
        .send(
            call(
                "alice",
                Method::PUT,
                format!("/v1/workspaces/{ws}/apps/{}/configuration", id(&echo)),
            )
            .naming(&northwind)
            .body(json!({ "mcp_server_url": "https://echo.example/mcp" })),
        )
        .await;
    assert_eq!(setup(&server, "alice", &northwind).await, (true, true, false));
    assert_eq!(
        setup(&server, "alice", &acme).await,
        (false, false, false),
        "nor Northwind's here"
    );

    // Acme Co: a catalog OAuth app is ready only with its client, and a revoked key doesn't count.
    let linear = created(
        &server,
        "alice",
        format!("/v1/organizations/{acme}/apps"),
        &acme,
        app("linear", "oauth2"),
    )
    .await;
    let path = format!(
        "/v1/organizations/{acme}/apps/{}/environments/production/configuration",
        id(&linear)
    );
    server
        .send(
            call("alice", Method::PUT, &path)
                .body(json!({ "mcp_server_url": "https://mcp.linear.example/mcp", "oauth_client_id": "c" })),
        )
        .await;
    assert_eq!(
        setup(&server, "alice", &acme).await,
        (true, false, false),
        "incomplete isn't ready"
    );
    server
        .send(call("alice", Method::PUT, &path).body(json!({ "oauth_client_secret": SECRET })))
        .await;
    assert_eq!(setup(&server, "alice", &acme).await, (true, true, false));
    let key = sandbox_key(&server, "alice", &acme).await;
    assert_eq!(setup(&server, "alice", &acme).await, (true, true, true));
    assert_eq!(
        setup(&server, "alice", &northwind).await,
        (true, true, false),
        "Acme's key isn't Northwind's"
    );
    server
        .send(call(
            "alice",
            Method::POST,
            format!("/v1/organizations/{acme}/api-keys/{key}/revoke"),
        ))
        .await;
    assert_eq!(
        setup(&server, "alice", &acme).await,
        (true, true, false),
        "a revoked key doesn't count"
    );
}

/// A workspace app's secret is bound to its workspace too.
#[tokio::test]
async fn seals_a_workspace_apps_secret_to_its_workspace() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let ws = server.workspace("alice", &org, "acme-sandbox", "sandbox").await;
    let ws = id(&ws).to_owned();
    let own = created(
        &server,
        "alice",
        format!("/v1/workspaces/{ws}/apps"),
        &org,
        app("crm", "oauth2"),
    )
    .await;
    let (status, body) = server
        .send(
            call("alice", Method::PUT, format!("/v1/workspaces/{ws}/apps/{}/configuration", id(&own)))
                .naming(&org)
                .body(json!({ "mcp_server_url": "https://crm.example/mcp", "oauth_client_id": "c", "oauth_client_secret": SECRET })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["status"], "ready");
    assert!(!body.to_string().contains(SECRET));

    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", server.directory.join("zupflow.sqlite").display()))
        .await
        .unwrap();
    let row: (String, Vec<u8>, Vec<u8>, Vec<u8>) = sqlx::query_as(
        "SELECT secret_key_id, secret_wrapped_key, secret_nonce, secret_ciphertext \
         FROM workspace_app_configurations WHERE organization_id = ? AND workspace_id = ? AND app_id = ?",
    )
    .bind(OrganizationId::parse(&org).unwrap().uuid())
    .bind(WorkspaceId::parse(&ws).unwrap().uuid())
    .bind(AppId::parse(id(&own)).unwrap().uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    let sealed = Sealed {
        wrapped_key: WrappedKey {
            key_id: row.0,
            bytes: row.1,
        },
        nonce: row.2.try_into().unwrap(),
        ciphertext: row.3,
    };
    let keys = LocalKeyService::from_server_secret(&[7; 32]);
    let (org_id, app_id) = (OrganizationId::parse(&org).unwrap(), AppId::parse(id(&own)).unwrap());
    let workspace_id = WorkspaceId::parse(&ws).unwrap();
    let context =
        |workspace| EncryptionContext::oauth_client_secret(org_id, workspace, Environment::Sandbox, app_id, 1);
    let opened = keys::open(&keys, &context(Some(workspace_id)), &sealed).await.unwrap();
    assert_eq!(opened.expose(), SECRET.as_bytes());
    for workspace in [None, Some(WorkspaceId::generate())] {
        assert_eq!(
            keys::open(&keys, &context(workspace), &sealed).await.unwrap_err(),
            KeyError::Refused,
            "{workspace:?}"
        );
    }
}
