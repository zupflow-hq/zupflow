//! Installations end to end (decision 0034): reached only through their own
//! workspace, managed by role, their credentials gone once disconnected or
//! deleted, and counted where the contract shows them. Connect makes them;
//! until it exists these tests make them through the store.

#![allow(clippy::unwrap_used)]

mod common;

use axum::http::{Method, StatusCode};
use common::{Server, call, code};
use serde_json::{Value, json};
use zupflow_basic::local_keys::LocalKeyService;
use zupflow_core::environment::Environment;
use zupflow_core::ids::{AppId, InstallationId, OrganizationId, WorkspaceId};
use zupflow_core::installations::{ConnectedAccount, InstallationStore, InstalledApp, NewCredential, NewInstallation};
use zupflow_core::keys::{self, EncryptionContext, KeyError};
use zupflow_sqlite::{Database, SqliteInstallationStore};

const CREDENTIAL: &[u8] = b"lin_api_plaintext_77";

fn app(key: &str, auth_type: &str) -> Value {
    json!({ "kind": "mcp", "name": key.to_uppercase(), "key": key, "description": "", "auth_type": auth_type })
}

async fn created(server: &Server, user: &str, path: String, org: &str, body: Value) -> Value {
    let (status, body) = server.send(call(user, Method::POST, path).naming(org).body(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["data"].clone()
}

fn id(value: &Value) -> &str {
    value["id"].as_str().unwrap()
}

async fn store(server: &Server) -> SqliteInstallationStore {
    SqliteInstallationStore::new(Database::open(&server.directory.join("zupflow.sqlite")).await.unwrap())
}

/// Connects `app` in `ws`, as Connect will: the credential sealed under the
/// installation's own context. Its ID.
async fn connect(server: &Server, org: &str, ws: &Value, app: InstalledApp, label: &str) -> String {
    let (organization_id, workspace_id) = (OrganizationId::parse(org).unwrap(), WorkspaceId::parse(id(ws)).unwrap());
    let environment = Environment::parse(ws["environment"].as_str().unwrap()).unwrap();
    let installation_id = InstallationId::generate();
    let context = EncryptionContext::installation_credential(
        organization_id,
        workspace_id,
        environment,
        app.id(),
        installation_id,
        1,
    );
    let sealed = keys::seal(&LocalKeyService::from_server_secret(&[7; 32]), &context, CREDENTIAL)
        .await
        .unwrap();
    let installation = NewInstallation {
        app,
        label: label.to_owned(),
        configuration_version: 1,
        account: ConnectedAccount {
            external_id: "4242".to_owned(),
            display_name: "Jordan".to_owned(),
            email: Some("jordan@example.com".to_owned()),
        },
        granted_scopes: vec!["read".to_owned()],
        expires_at: None,
        credential: NewCredential { sealed, version: 1 },
        snapshot: common::token_snapshot(),
    };
    store(server)
        .await
        .create_installation(organization_id, workspace_id, installation_id, &installation)
        .await
        .unwrap()
        .id
        .to_string()
}

struct Tenant {
    org: String,
    sandbox: Value,
    production: Value,
    catalog: Value,
    own: Value,
}

async fn tenant(server: &Server) -> Tenant {
    let org = server.organization("alice", "northwind").await;
    let sandbox = server.workspace("alice", &org, "acme-sandbox", "sandbox").await;
    let production = server.workspace("alice", &org, "acme", "production").await;
    let catalog = created(
        server,
        "alice",
        format!("/v1/organizations/{org}/apps"),
        &org,
        app("linear", "oauth2"),
    )
    .await;
    let own = created(
        server,
        "alice",
        format!("/v1/workspaces/{}/apps", id(&sandbox)),
        &org,
        app("echo", "token"),
    )
    .await;
    Tenant {
        org,
        sandbox,
        production,
        catalog,
        own,
    }
}

fn catalog(t: &Tenant) -> InstalledApp {
    InstalledApp::Catalog(AppId::parse(id(&t.catalog)).unwrap())
}

fn own(t: &Tenant) -> InstalledApp {
    InstalledApp::Workspace(AppId::parse(id(&t.own)).unwrap())
}

#[tokio::test]
async fn shows_renames_disconnects_and_deletes_an_installation() {
    let server = Server::new().await;
    let t = tenant(&server).await;
    let ws = id(&t.sandbox);
    let first = connect(&server, &t.org, &t.sandbox, catalog(&t), "Engineering").await;
    let second = connect(&server, &t.org, &t.sandbox, own(&t), "Echo").await;

    let (status, list) = server
        .send(call("alice", Method::GET, format!("/v1/workspaces/{ws}/installations")).naming(&t.org))
        .await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = list["data"].as_array().unwrap().iter().map(id).collect();
    assert_eq!(ids, [second.as_str(), first.as_str()], "newest first");

    let path = format!("/v1/workspaces/{ws}/installations/{first}");
    let (_, shown) = server.send(call("alice", Method::GET, &path).naming(&t.org)).await;
    let shown = &shown["data"];
    assert_eq!(
        shown["app"],
        json!({
            "id": id(&t.catalog), "name": "LINEAR", "key": "linear", "owner_scope": "organization",
            "icon": t.catalog["icon"], "auth_type": "oauth2",
        })
    );
    assert_eq!(
        (
            &shown["label"],
            &shown["status"],
            &shown["grant_count"],
            &shown["workspace_id"]
        ),
        (&json!("Engineering"), &json!("connected"), &json!(0), &json!(ws))
    );
    assert_eq!(shown["connection"]["account"]["external_id"], "4242");
    assert_eq!(shown["connection"]["credential_version"], 1);
    assert!(
        !list.to_string().contains("plaintext_77"),
        "no credential in a response"
    );
    assert_eq!(list["data"][0]["app"]["owner_scope"], "workspace");
    // Each has a short ID of its own in the workspace (decision 0047), read-only.
    let short_id = shown["short_id"].as_str().unwrap().to_owned();
    assert!(
        short_id.len() == 6
            && short_id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b)),
        "{short_id}"
    );
    assert_ne!(list["data"][0]["short_id"], shown["short_id"]);

    let (status, renamed) = server
        .send(
            call("alice", Method::PATCH, &path)
                .naming(&t.org)
                .body(json!({ "label": "  Platform  " })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{renamed}");
    assert_eq!(renamed["data"]["label"], "Platform");
    assert_eq!(renamed["data"]["short_id"], short_id.as_str(), "kept across a rename");
    let (status, kept) = server
        .send(
            call("alice", Method::PATCH, &path)
                .naming(&t.org)
                .body(json!({ "label": "Platform", "short_id": "aaaaaa" })),
        )
        .await;
    assert_eq!(
        (status, &kept["data"]["short_id"]),
        (StatusCode::OK, &json!(short_id)),
        "never set by a client"
    );
    for (label, message) in [
        (json!(""), "Required."),
        (json!("x".repeat(41)), "Use 40 characters or fewer."),
    ] {
        let (status, body) = server
            .send(
                call("alice", Method::PATCH, &path)
                    .naming(&t.org)
                    .body(json!({ "label": label })),
            )
            .await;
        assert_eq!(
            (status, &body["error"]["fields"]["label"]),
            (StatusCode::BAD_REQUEST, &json!(message))
        );
    }

    // Disconnecting keeps the installation and forgets its credential.
    let (organization_id, workspace_id) = (OrganizationId::parse(&t.org).unwrap(), WorkspaceId::parse(ws).unwrap());
    let first_id = InstallationId::parse(&first).unwrap();
    let credential = store(&server)
        .await
        .find_credential(organization_id, workspace_id, first_id)
        .await
        .unwrap()
        .unwrap();
    let keys = LocalKeyService::from_server_secret(&[7; 32]);
    let context = |workspace| {
        EncryptionContext::installation_credential(
            organization_id,
            workspace,
            Environment::Sandbox,
            AppId::parse(id(&t.catalog)).unwrap(),
            first_id,
            1,
        )
    };
    assert_eq!(
        keys::open(&keys, &context(workspace_id), &credential.sealed)
            .await
            .unwrap()
            .expose(),
        CREDENTIAL
    );
    assert_eq!(
        keys::open(
            &keys,
            &context(WorkspaceId::parse(id(&t.production)).unwrap()),
            &credential.sealed
        )
        .await
        .unwrap_err(),
        KeyError::Refused,
        "sealed to its own workspace"
    );
    let (status, disconnected) = server
        .send(call("alice", Method::POST, format!("{path}/disconnect")).naming(&t.org))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(disconnected["data"]["status"], "disconnected");
    assert_eq!(
        disconnected["data"]["short_id"],
        short_id.as_str(),
        "kept across a disconnect"
    );
    assert_eq!(disconnected["data"]["connection"]["account"]["external_id"], "4242");
    assert_eq!(
        store(&server)
            .await
            .find_credential(organization_id, workspace_id, first_id)
            .await
            .unwrap(),
        None
    );

    let (status, _) = server.send(call("alice", Method::DELETE, &path).naming(&t.org)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = server.send(call("alice", Method::GET, &path).naming(&t.org)).await;
    assert_eq!(
        (status, &body["error"]["message"]),
        (StatusCode::NOT_FOUND, &json!("Installation was not found."))
    );
}

#[tokio::test]
async fn lets_developers_manage_installations_and_admins_delete_them() {
    let server = Server::new().await;
    let t = tenant(&server).await;
    let ws = id(&t.sandbox);
    server.member("alice", &t.org, "devin", "developer").await;
    server.member("alice", &t.org, "vic", "viewer").await;
    let installation = connect(&server, &t.org, &t.sandbox, catalog(&t), "Engineering").await;
    let path = format!("/v1/workspaces/{ws}/installations/{installation}");

    let (status, _) = server.send(call("vic", Method::GET, &path).naming(&t.org)).await;
    assert_eq!(status, StatusCode::OK);
    for (user, method, path) in [
        ("vic", Method::PATCH, path.clone()),
        ("vic", Method::POST, format!("{path}/disconnect")),
        ("vic", Method::DELETE, path.clone()),
        ("devin", Method::DELETE, path.clone()),
    ] {
        let (status, body) = server
            .send(
                call(user, method.clone(), &path)
                    .naming(&t.org)
                    .body(json!({ "label": "Mine" })),
            )
            .await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::FORBIDDEN, "forbidden"),
            "{user} {method}"
        );
    }
    let (status, _) = server
        .send(
            call("devin", Method::PATCH, &path)
                .naming(&t.org)
                .body(json!({ "label": "Devin's" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = server
        .send(call("devin", Method::POST, format!("{path}/disconnect")).naming(&t.org))
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn keeps_installations_in_their_own_workspace() {
    let server = Server::new().await;
    let t = tenant(&server).await;
    let other = server.organization("riley", "larkspur").await;
    let theirs = server.workspace("riley", &other, "harbor", "sandbox").await;
    let (ws, production) = (id(&t.sandbox), id(&t.production));
    let installation = connect(&server, &t.org, &t.sandbox, catalog(&t), "Engineering").await;

    for (user, organization, base) in [
        (
            "alice",
            &t.org,
            format!("/v1/workspaces/{production}/installations/{installation}"),
        ),
        (
            "riley",
            &other,
            format!("/v1/workspaces/{}/installations/{installation}", id(&theirs)),
        ),
        (
            "riley",
            &t.org,
            format!("/v1/workspaces/{ws}/installations/{installation}"),
        ),
        ("alice", &t.org, format!("/v1/workspaces/{ws}/installations/{ws}")),
    ] {
        for (method, path) in [
            (Method::GET, base.clone()),
            (Method::PATCH, base.clone()),
            (Method::POST, format!("{base}/disconnect")),
            (Method::DELETE, base.clone()),
        ] {
            let (status, body) = server
                .send(
                    call(user, method.clone(), &path)
                        .naming(organization)
                        .body(json!({ "label": "Stolen" })),
                )
                .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{user} {method} {path}: {body}");
        }
    }
    let (_, listed) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("/v1/workspaces/{production}/installations"),
            )
            .naming(&t.org),
        )
        .await;
    assert_eq!(listed["data"], json!([]));
    let (_, still) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("/v1/workspaces/{ws}/installations/{installation}"),
            )
            .naming(&t.org),
        )
        .await;
    assert_eq!(
        (&still["data"]["label"], &still["data"]["status"]),
        (&json!("Engineering"), &json!("connected")),
        "nothing reached through the wrong routes changed"
    );

    // A key never reaches the other environment's workspaces.
    let (_, key) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{}/api-keys", t.org))
                .body(json!({ "name": "Server", "environment": "production" })),
        )
        .await;
    let key = key["data"]["secret"].as_str().unwrap();
    let (status, _) = server
        .send(call(
            key,
            Method::GET,
            format!("/v1/workspaces/{ws}/installations/{installation}"),
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn lists_a_catalog_apps_installations_across_workspaces() {
    let server = Server::new().await;
    let t = tenant(&server).await;
    let mut made = Vec::new();
    for ws in [&t.sandbox, &t.production, &t.sandbox] {
        made.push(connect(&server, &t.org, ws, catalog(&t), "Team").await);
    }
    connect(&server, &t.org, &t.sandbox, own(&t), "Echo").await;
    let path = format!("/v1/organizations/{}/apps/{}/installations", t.org, id(&t.catalog));

    let (status, first) = server.send(call("alice", Method::GET, format!("{path}?limit=2"))).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["meta"]["pagination"]["total"], 3);
    assert_eq!(first["data"].as_array().unwrap().len(), 2);
    let item = &first["data"][0];
    assert_eq!(item["installation"]["id"], made[2].as_str(), "newest first");
    assert_eq!(
        item["workspace"],
        json!({ "id": id(&t.sandbox), "name": "ACME-SANDBOX", "slug": "acme-sandbox", "environment": "sandbox" })
    );
    let cursor = first["meta"]["pagination"]["next_cursor"].as_str().unwrap();
    let (_, second) = server
        .send(call("alice", Method::GET, format!("{path}?limit=2&cursor={cursor}")))
        .await;
    assert_eq!(second["data"].as_array().unwrap().len(), 1);
    assert_eq!(second["data"][0]["installation"]["id"], made[0].as_str());
    assert_eq!(second["meta"]["pagination"]["next_cursor"], Value::Null);

    let (_, production) = server
        .send(call("alice", Method::GET, format!("{path}?environment=production")))
        .await;
    assert_eq!(production["meta"]["pagination"]["total"], 1);
    let (status, body) = server
        .send(call("alice", Method::GET, format!("{path}?cursor=forged")))
        .await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, "validation_failed"));

    // A Sandbox key sees Sandbox only.
    let (_, key) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{}/api-keys", t.org))
                .body(json!({ "name": "Server", "environment": "sandbox" })),
        )
        .await;
    let key = key["data"]["secret"].as_str().unwrap();
    let (_, own_env) = server.send(call(key, Method::GET, &path)).await;
    assert_eq!(own_env["meta"]["pagination"]["total"], 2);
    let (status, body) = server
        .send(call(key, Method::GET, format!("{path}?environment=production")))
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "environment_mismatch")
    );

    // Never another organization's, and never a workspace app's.
    server.organization("riley", "larkspur").await;
    let (status, _) = server.send(call("riley", Method::GET, &path)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = server
        .send(call(
            "alice",
            Method::GET,
            format!("/v1/organizations/{}/apps/{}/installations", t.org, id(&t.own)),
        ))
        .await;
    assert_eq!(
        (status, &body["error"]["message"]),
        (StatusCode::NOT_FOUND, &json!("App was not found."))
    );
}

#[tokio::test]
async fn counts_installations_where_the_contract_shows_them() {
    let server = Server::new().await;
    let t = tenant(&server).await;
    let other = server.organization("riley", "larkspur").await;
    let harbor = server.workspace("riley", &other, "harbor", "sandbox").await;
    let theirs = created(
        &server,
        "riley",
        format!("/v1/organizations/{other}/apps"),
        &other,
        app("linear", "oauth2"),
    )
    .await;
    connect(
        &server,
        &other,
        &harbor,
        InstalledApp::Catalog(AppId::parse(id(&theirs)).unwrap()),
        "Harbor",
    )
    .await;

    let has_installation = || async {
        let (_, body) = server
            .send(call("alice", Method::GET, format!("/v1/organizations/{}", t.org)))
            .await;
        body["data"]["setup"]["has_installation"].as_bool().unwrap()
    };
    assert!(!has_installation().await, "another organization's doesn't count");
    let installation = connect(&server, &t.org, &t.sandbox, catalog(&t), "Team").await;
    connect(&server, &t.org, &t.production, catalog(&t), "Team").await;
    let echo = connect(&server, &t.org, &t.sandbox, own(&t), "Echo").await;
    assert!(has_installation().await);

    let (_, catalog_app) = server
        .send(call(
            "alice",
            Method::GET,
            format!("/v1/organizations/{}/apps/{}", t.org, id(&t.catalog)),
        ))
        .await;
    assert_eq!(catalog_app["data"]["installation_count"], 2);
    // A key counts its own environment's only.
    let (_, key) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{}/api-keys", t.org))
                .body(json!({ "name": "Server", "environment": "sandbox" })),
        )
        .await;
    let key = key["data"]["secret"].as_str().unwrap();
    let (_, seen_by_key) = server
        .send(call(
            key,
            Method::GET,
            format!("/v1/organizations/{}/apps/{}", t.org, id(&t.catalog)),
        ))
        .await;
    assert_eq!(seen_by_key["data"]["installation_count"], 1);
    let (_, own_app) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("/v1/workspaces/{}/apps/{}", id(&t.sandbox), id(&t.own)),
            )
            .naming(&t.org),
        )
        .await;
    assert_eq!(own_app["data"]["installation_count"], 1);
    let (_, workspace) = server
        .send(call("alice", Method::GET, format!("/v1/workspaces/{}", id(&t.sandbox))).naming(&t.org))
        .await;
    assert_eq!(workspace["data"]["stats"]["installations"], 2);

    // Only connected installations count as set up.
    let (_, listed) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("/v1/workspaces/{}/installations", id(&t.production)),
            )
            .naming(&t.org),
        )
        .await;
    let production = listed["data"][0]["id"].as_str().unwrap().to_owned();
    for (ws, inst) in [
        (id(&t.sandbox), installation.as_str()),
        (id(&t.production), production.as_str()),
    ] {
        server
            .send(
                call(
                    "alice",
                    Method::POST,
                    format!("/v1/workspaces/{ws}/installations/{inst}/disconnect"),
                )
                .naming(&t.org),
            )
            .await;
    }
    assert!(has_installation().await, "Echo is still connected");
    server
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/workspaces/{}/installations/{echo}/disconnect", id(&t.sandbox)),
            )
            .naming(&t.org),
        )
        .await;
    assert!(!has_installation().await, "disconnected installations don't count");
}
