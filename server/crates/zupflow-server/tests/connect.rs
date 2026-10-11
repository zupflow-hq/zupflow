//! Connect end to end (decision 0005): settings per organization and
//! environment; sessions started only for ready apps of this workspace, with a
//! link shown once; the hosted page reached by the link's reference alone.

#![allow(clippy::unwrap_used)]

mod common;

use axum::http::{Method, StatusCode};
use common::{Server, call, code};
use serde_json::{Value, json};

fn id(value: &Value) -> &str {
    value["id"].as_str().unwrap()
}

struct Tenant {
    org: String,
    sandbox: String,
    production: String,
    /// A ready token app in the catalog.
    ready: String,
    /// A catalog app with no configuration.
    unready: String,
}

async fn tenant(server: &Server, user: &str, slug: &str) -> Tenant {
    let org = server.organization(user, slug).await;
    let sandbox = server
        .workspace(user, &org, &format!("{slug}-sandbox"), "sandbox")
        .await;
    let production = server.workspace(user, &org, slug, "production").await;
    let mut apps = Vec::new();
    for app_slug in ["tools", "notes"] {
        let (status, body) = server
            .send(
                call(user, Method::POST, format!("/v1/organizations/{org}/apps")).body(json!({
                    "kind": "mcp", "name": app_slug, "slug": app_slug, "description": "", "auth_type": "token",
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        apps.push(id(&body["data"]).to_owned());
    }
    for environment in ["sandbox", "production"] {
        let (status, _) = server
            .send(
                call(
                    user,
                    Method::PUT,
                    format!(
                        "/v1/organizations/{org}/apps/{}/environments/{environment}/configuration",
                        apps[0]
                    ),
                )
                .body(json!({ "mcp_server_url": "https://tools.example/mcp" })),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    Tenant {
        org,
        sandbox: id(&sandbox).to_owned(),
        production: id(&production).to_owned(),
        ready: apps[0].clone(),
        unready: apps[1].clone(),
    }
}

fn session(app: &str, return_url: Value) -> Value {
    json!({ "app_id": app, "intent": "create", "label": "Main", "installation_id": null, "return_url": return_url })
}

fn start<'a>(user: &'a str, org: &'a str, ws: &str, body: Value) -> common::Call<'a> {
    call(user, Method::POST, format!("/v1/workspaces/{ws}/connect-sessions"))
        .naming(org)
        .body(body)
}

/// The reference at the end of a created session's link.
fn reference_of(created: &Value) -> String {
    created["url"].as_str().unwrap().rsplit('/').next().unwrap().to_owned()
}

async fn key(server: &Server, user: &str, org: &str, environment: &str) -> String {
    let (_, body) = server
        .send(
            call(user, Method::POST, format!("/v1/organizations/{org}/api-keys"))
                .body(json!({ "name": "Server", "environment": environment })),
        )
        .await;
    body["data"]["secret"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn keeps_settings_per_organization_and_environment() {
    let server = Server::new().await;
    let t = tenant(&server, "alice", "northwind").await;
    let path = |env: &str| format!("/v1/organizations/{}/connect-settings/{env}", t.org);

    let (status, empty) = server.send(call("alice", Method::GET, path("sandbox"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        empty["data"],
        json!({ "environment": "sandbox", "return_urls": [], "allowed_origins": [], "updated_at": null })
    );

    let (status, body) = server
        .send(call("alice", Method::PUT, path("production")).body(json!({
            "return_urls": ["https://app.example/*", "http://localhost:3000/cb", "https://app.example/cb#x", "nope"],
            "allowed_origins": ["https://app.example/path", "http://localhost:3000"],
        })))
        .await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, "validation_failed"));
    assert_eq!(
        body["error"]["fields"],
        json!({
            "return_urls.0": "Wildcards aren't allowed. List each URL exactly.",
            "return_urls.1": "Use https:// (http://localhost is allowed in Sandbox).",
            "return_urls.2": "Remove the #fragment.",
            "return_urls.3": "Enter a full URL.",
            "allowed_origins.0": "Use an origin only: scheme, host and port.",
            "allowed_origins.1": "Use https:// (http://localhost is allowed in Sandbox).",
        })
    );

    let (status, saved) = server
        .send(call("alice", Method::PUT, path("sandbox")).body(json!({
            "return_urls": ["http://localhost:3000/cb", "http://localhost:3000/cb", "https://app.example/cb"],
            "allowed_origins": ["https://app.example/", "http://localhost:3000"],
        })))
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(
        (&saved["data"]["return_urls"], &saved["data"]["allowed_origins"]),
        (
            &json!(["http://localhost:3000/cb", "https://app.example/cb"]),
            &json!(["https://app.example", "http://localhost:3000"])
        ),
        "duplicates go, a trailing slash goes"
    );
    let (_, production) = server.send(call("alice", Method::GET, path("production"))).await;
    assert_eq!(production["data"]["return_urls"], json!([]), "Production is its own");

    // Roles, keys and other organizations.
    server.member("alice", &t.org, "devin", "developer").await;
    let (status, _) = server
        .send(call("devin", Method::PUT, path("sandbox")).body(json!({ "return_urls": [], "allowed_origins": [] })))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let sandbox_key = key(&server, "alice", &t.org, "sandbox").await;
    let (status, body) = server.send(call(&sandbox_key, Method::GET, path("production"))).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "environment_mismatch")
    );
    server.organization("riley", "larkspur").await;
    let (status, _) = server.send(call("riley", Method::GET, path("sandbox"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn starts_a_session_only_for_a_ready_app_of_this_workspace() {
    let server = Server::new().await;
    let t = tenant(&server, "alice", "northwind").await;
    let sessions = |ws: &str| format!("/v1/workspaces/{ws}/connect-sessions");
    let (status, created) = server
        .send(start("alice", &t.org, &t.sandbox, session(&t.ready, Value::Null)))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let created = &created["data"];
    let url = created["url"].as_str().unwrap();
    assert!(url.starts_with("http://localhost:8080/connect/"), "{url}");
    let reference = reference_of(created);
    assert_eq!(reference.len(), 32);
    assert!(reference.bytes().all(|b| b.is_ascii_alphanumeric()));
    assert_eq!(
        (
            &created["status"],
            &created["intent"],
            &created["environment"],
            &created["auth"]
        ),
        (&json!("pending"), &json!("create"), &json!("sandbox"), &Value::Null)
    );
    assert_eq!(created["app"]["id"], t.ready.as_str());

    // The link is shown once: no read returns it, nor its reference.
    let (_, listed) = server
        .send(call("alice", Method::GET, sessions(&t.sandbox)).naming(&t.org))
        .await;
    let (_, got) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("{}/{}", sessions(&t.sandbox), id(created)),
            )
            .naming(&t.org),
        )
        .await;
    for body in [&listed, &got] {
        assert!(!body.to_string().contains(&reference));
        assert!(!body.to_string().contains("\"url\""));
    }
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
    // Nor does the database hold it.
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", server.directory.join("zupflow.sqlite").display()))
        .await
        .unwrap();
    let rows: Vec<(Vec<u8>,)> = sqlx::query_as("SELECT reference_hash FROM connect_sessions")
        .fetch_all(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert!(
        rows.iter()
            .all(|(hash,)| hash.len() == 32 && !hash.windows(8).any(|w| w == &reference.as_bytes()[..8]))
    );

    // Refusals, in the order the mock gives them.
    for (body, status, expected) in [
        (
            session(&t.unready, Value::Null),
            StatusCode::UNPROCESSABLE_ENTITY,
            "configuration_not_ready",
        ),
        (session("app_nothere", Value::Null), StatusCode::NOT_FOUND, "not_found"),
        (
            session(&t.ready, json!("https://evil.example/cb")),
            StatusCode::UNPROCESSABLE_ENTITY,
            "return_url_not_allowed",
        ),
        (
            json!({ "app_id": t.ready, "intent": "create", "label": "Main", "installation_id": "inst_x", "return_url": null }),
            StatusCode::BAD_REQUEST,
            "validation_failed",
        ),
        (
            json!({ "app_id": t.ready, "intent": "reconnect", "label": "Main", "installation_id": null, "return_url": null }),
            StatusCode::BAD_REQUEST,
            "validation_failed",
        ),
        (
            json!({ "app_id": t.ready, "intent": "reconnect", "label": "Main", "installation_id": "inst_nothere", "return_url": null }),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
    ] {
        let (got, error) = server.send(start("alice", &t.org, &t.sandbox, body.clone())).await;
        assert_eq!((got, code(&error)), (status, expected), "{body}");
    }

    // The Console only observes Production; a key there starts sessions.
    let (status, body) = server
        .send(start("alice", &t.org, &t.production, session(&t.ready, Value::Null)))
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "console_read_only_in_production")
    );
    let production_key = key(&server, "alice", &t.org, "production").await;
    let (status, _) = server
        .send(call(&production_key, Method::POST, sessions(&t.production)).body(session(&t.ready, Value::Null)))
        .await;
    assert_eq!(status, StatusCode::CREATED);

    // Viewers don't start sessions; another organization doesn't see them.
    server.member("alice", &t.org, "vic", "viewer").await;
    let (status, _) = server
        .send(start("vic", &t.org, &t.sandbox, session(&t.ready, Value::Null)))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let other = tenant(&server, "riley", "larkspur").await;
    let (status, _) = server
        .send(
            call(
                "riley",
                Method::GET,
                format!("{}/{}", sessions(&t.sandbox), id(created)),
            )
            .naming(&other.org),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = server
        .send(
            call(
                "riley",
                Method::GET,
                format!("{}/{}", sessions(&other.sandbox), id(created)),
            )
            .naming(&other.org),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Another organization's ready app isn't this workspace's to connect.
    let (status, _) = server
        .send(start(
            "riley",
            &other.org,
            &other.sandbox,
            session(&t.ready, Value::Null),
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn serves_the_hosted_page_by_its_reference_alone() {
    let server = Server::new().await;
    let t = tenant(&server, "alice", "northwind").await;
    let start = || {
        call(
            "alice",
            Method::POST,
            format!("/v1/workspaces/{}/connect-sessions", t.sandbox),
        )
        .naming(&t.org)
        .body(session(&t.ready, Value::Null))
    };
    let (_, created) = server.send(start()).await;
    let reference = reference_of(&created["data"]);

    // Whoever calls: the reference is the authority, not a credential.
    let (status, hosted) = server
        .send(call("nobody", Method::GET, format!("/v1/connect/sessions/{reference}")))
        .await;
    assert_eq!(status, StatusCode::OK, "{hosted}");
    assert_eq!(hosted["data"]["id"], created["data"]["id"]);
    assert_eq!(
        hosted["data"]["auth"]["type"], "token",
        "the page gets the template to render"
    );
    assert!(!hosted.to_string().contains("\"url\""));
    for unknown in ["nope", &reference[1..], &"x".repeat(200)] {
        let (status, body) = server
            .send(call("nobody", Method::GET, format!("/v1/connect/sessions/{unknown}")))
            .await;
        assert_eq!(
            (status, &body["error"]["message"]),
            (StatusCode::NOT_FOUND, &json!("Connect session was not found."))
        );
    }

    // Cancel once. A retry with the same key replays that answer (the key is
    // the session's); a fresh attempt is 409.
    let cancel = |key: &'static str| {
        call(
            "nobody",
            Method::POST,
            format!("/v1/connect/sessions/{reference}/cancel"),
        )
        .idempotent(key)
    };
    let (status, cancelled) = server.send(cancel("k-1")).await;
    assert_eq!(
        (status, &cancelled["data"]["status"]),
        (StatusCode::OK, &json!("cancelled"))
    );
    assert_eq!(
        server.send(cancel("k-1")).await,
        (status, cancelled.clone()),
        "replayed"
    );
    let (status, again) = server.send(cancel("k-2")).await;
    assert_eq!((status, code(&again)), (StatusCode::CONFLICT, "session_not_pending"));
    assert_eq!(
        again["error"]["message"],
        "This Connect session was already used. Start again from the app."
    );
    // The same key on another session is that session's own.
    let (_, other) = server.send(start()).await;
    let other_reference = reference_of(&other["data"]);
    let (status, body) = server
        .send(
            call(
                "nobody",
                Method::POST,
                format!("/v1/connect/sessions/{other_reference}/cancel"),
            )
            .idempotent("k-1"),
        )
        .await;
    assert_eq!((status, &body["data"]["id"]), (StatusCode::OK, &other["data"]["id"]));
    // An unknown reference is the same 404, key or not.
    let (status, _) = server
        .send(call("nobody", Method::POST, "/v1/connect/sessions/nope/cancel").idempotent("k-1"))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, seen) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("/v1/workspaces/{}/connect-sessions/{}", t.sandbox, id(&created["data"])),
            )
            .naming(&t.org),
        )
        .await;
    assert_eq!(seen["data"]["status"], "cancelled");
}

/// An installation of `app` in `ws`, made through the store: Connect is what
/// makes them, and the credential submit arrives next.
async fn installation(server: &Server, org: &str, ws: &str, app: &str, label: &str) -> String {
    use zupflow_core::ids::{AppId, InstallationId, OrganizationId, WorkspaceId};
    use zupflow_core::installations::{
        ConnectedAccount, InstallationStore, InstalledApp, NewCredential, NewInstallation,
    };
    use zupflow_core::keys::{Sealed, WrappedKey};
    let store = zupflow_sqlite::SqliteInstallationStore::new(
        zupflow_sqlite::Database::open(&server.directory.join("zupflow.sqlite"))
            .await
            .unwrap(),
    );
    let installation = NewInstallation {
        app: InstalledApp::Catalog(AppId::parse(app).unwrap()),
        label: label.to_owned(),
        configuration_version: 1,
        account: ConnectedAccount {
            external_id: "4242".to_owned(),
            display_name: "Jordan".to_owned(),
            email: None,
        },
        granted_scopes: Vec::new(),
        expires_at: None,
        credential: NewCredential {
            sealed: Sealed {
                wrapped_key: WrappedKey {
                    key_id: "k".to_owned(),
                    bytes: vec![1; 40],
                },
                nonce: [1; 12],
                ciphertext: vec![1; 16],
            },
            version: 1,
        },
        snapshot: common::token_snapshot(),
    };
    store
        .create_installation(
            OrganizationId::parse(org).unwrap(),
            WorkspaceId::parse(ws).unwrap(),
            InstallationId::generate(),
            &installation,
        )
        .await
        .unwrap()
        .id
        .to_string()
}

/// A session uses its workspace's own environment, and only its own
/// workspace's installations; it expires on every read.
#[tokio::test]
async fn keeps_sessions_to_their_own_environment_and_workspace() {
    let server = Server::new().await;
    let t = tenant(&server, "alice", "northwind").await;
    let sessions = |ws: &str| format!("/v1/workspaces/{ws}/connect-sessions");
    let production_key = key(&server, "alice", &t.org, "production").await;
    let from_production = |body: Value| call(&production_key, Method::POST, sessions(&t.production)).body(body);

    // An app ready only in Sandbox isn't ready for a Production workspace.
    let (status, _) = server
        .send(
            call(
                "alice",
                Method::PUT,
                format!(
                    "/v1/organizations/{}/apps/{}/environments/sandbox/configuration",
                    t.org, t.unready
                ),
            )
            .body(json!({ "mcp_server_url": "https://notes.example/mcp" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = server.send(from_production(session(&t.unready, Value::Null))).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "configuration_not_ready")
    );
    let (status, _) = server
        .send(start("alice", &t.org, &t.sandbox, session(&t.unready, Value::Null)))
        .await;
    assert_eq!(status, StatusCode::CREATED, "ready in Sandbox");

    // A return URL allowed only in Sandbox isn't allowed from Production.
    let settings = |env: &str, url: &str| {
        call(
            "alice",
            Method::PUT,
            format!("/v1/organizations/{}/connect-settings/{env}", t.org),
        )
        .body(json!({ "return_urls": [url], "allowed_origins": [] }))
    };
    server.send(settings("sandbox", "http://localhost:3000/cb")).await;
    server.send(settings("production", "https://app.example/cb")).await;
    let (status, body) = server
        .send(from_production(session(&t.ready, json!("http://localhost:3000/cb"))))
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "return_url_not_allowed")
    );
    let (status, _) = server
        .send(from_production(session(&t.ready, json!("https://app.example/cb"))))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let long = format!("https://app.example/{}", "a".repeat(2000));
    let (status, body) = server.send(settings("production", &long)).await;
    assert_eq!(
        (status, &body["error"]["fields"]["return_urls.0"]),
        (StatusCode::BAD_REQUEST, &json!("Use 2,000 characters or fewer."))
    );

    // A reconnect renews this workspace's installation of this app, and nothing else.
    let other = server.workspace("alice", &t.org, "qa", "sandbox").await;
    let other = id(&other).to_owned();
    let installed = installation(&server, &t.org, &t.sandbox, &t.ready, "Engineering").await;
    let reconnect = |app: &str, installation: &str| json!({ "app_id": app, "intent": "reconnect", "label": "ignored", "installation_id": installation, "return_url": null });
    let (status, body) = server
        .send(start("alice", &t.org, &t.sandbox, reconnect(&t.unready, &installed)))
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "app_mismatch")
    );
    let (status, body) = server
        .send(start("alice", &t.org, &other, reconnect(&t.ready, &installed)))
        .await;
    assert_eq!(
        (status, &body["error"]["message"]),
        (StatusCode::NOT_FOUND, &json!("Installation was not found.")),
        "another workspace's installation"
    );
    let (status, renewed) = server
        .send(start("alice", &t.org, &t.sandbox, reconnect(&t.ready, &installed)))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{renewed}");
    assert_eq!(
        (
            &renewed["data"]["label"],
            &renewed["data"]["installation_id"],
            &renewed["data"]["expected_account"]["external_id"]
        ),
        (&json!("Engineering"), &json!(installed), &json!("4242")),
        "the installation's own label and account"
    );
    let (status, _) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("{}/{}", sessions(&other), id(&renewed["data"])),
            )
            .naming(&t.org),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "read through another workspace");

    // Past its expiry a session reads as expired to its workspace, and can't be cancelled.
    let reference = reference_of(&renewed["data"]);
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", server.directory.join("zupflow.sqlite").display()))
        .await
        .unwrap();
    sqlx::query("UPDATE connect_sessions SET expires_at = 0")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let (_, read) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("{}/{}", sessions(&t.sandbox), id(&renewed["data"])),
            )
            .naming(&t.org),
        )
        .await;
    assert_eq!(read["data"]["status"], "expired");
    // The link shows nothing: the same 404 as one that names no session.
    let (status, hosted) = server
        .send(call("nobody", Method::GET, format!("/v1/connect/sessions/{reference}")))
        .await;
    assert_eq!((status, code(&hosted)), (StatusCode::NOT_FOUND, "not_found"));
    let (status, body) = server
        .send(call(
            "nobody",
            Method::POST,
            format!("/v1/connect/sessions/{reference}/cancel"),
        ))
        .await;
    assert_eq!(
        (status, &body["error"]["message"]),
        (
            StatusCode::CONFLICT,
            &json!("This Connect link has expired. Start again from the app.")
        )
    );
}

// --- Submitting a token app's credentials -----------------------------------

const VERIFY: &str = "https://verify.example/me";

/// Gives the catalog app `app` a template with two inputs, the first a
/// patterned key sent in `X-Api-Key`, verified at [`VERIFY`].
async fn two_inputs(server: &Server, org: &str, app: &str) {
    let (status, body) = server
        .send(
            call("alice", Method::PATCH, format!("/v1/organizations/{org}/apps/{app}")).body(json!({ "auth": {
                "type": "token",
                "inputs": [
                    { "key": "api_key", "label": "API key", "secret": true, "help": null, "placeholder": null, "pattern": "^tk_[a-z0-9]{8,}$" },
                    { "key": "account", "label": "Account ID", "secret": false, "help": null, "placeholder": null, "pattern": null },
                ],
                "placement": { "location": "header", "name": "X-Api-Key", "prefix": "" },
                "verify_url": VERIFY, "instructions": "", "docs_url": null,
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

fn submit(reference: &str, values: Value) -> common::Call<'static> {
    call(
        "nobody",
        Method::POST,
        format!("/v1/connect/sessions/{reference}/credentials"),
    )
    .body(json!({ "values": values }))
}

async fn database(server: &Server) -> sqlx::SqlitePool {
    sqlx::SqlitePool::connect(&format!("sqlite:{}", server.directory.join("zupflow.sqlite").display()))
        .await
        .unwrap()
}

/// The installation's stored credential: its version and what it opens to
/// under its own context.
async fn credential(server: &Server, org: &str, ws: &str, app: &str, installation: &str) -> (i64, Value) {
    use zupflow_core::environment::Environment;
    use zupflow_core::ids::{AppId, InstallationId, OrganizationId, WorkspaceId};
    use zupflow_core::keys::{self, EncryptionContext, Sealed, WrappedKey};
    let pool = database(server).await;
    let installation_id = InstallationId::parse(installation).unwrap();
    let (version, key_id, wrapped, nonce, ciphertext): (i64, String, Vec<u8>, Vec<u8>, Vec<u8>) = sqlx::query_as(
        "SELECT version, key_id, wrapped_key, nonce, ciphertext FROM installation_credentials WHERE installation_id = ?",
    )
    .bind(installation_id.uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    let sealed = Sealed {
        wrapped_key: WrappedKey { key_id, bytes: wrapped },
        nonce: nonce.try_into().unwrap(),
        ciphertext,
    };
    let context = EncryptionContext::installation_credential(
        OrganizationId::parse(org).unwrap(),
        WorkspaceId::parse(ws).unwrap(),
        Environment::Sandbox,
        AppId::parse(app).unwrap(),
        installation_id,
        u32::try_from(version).unwrap(),
    );
    let keys = zupflow_basic::local_keys::LocalKeyService::from_server_secret(&[7; 32]);
    let opened = keys::open(&keys, &context, &sealed).await.unwrap();
    (version, serde_json::from_slice(opened.expose()).unwrap())
}

#[tokio::test]
async fn completes_a_token_session_once_with_a_verified_credential() {
    let server = Server::new().await;
    let t = tenant(&server, "alice", "northwind").await;
    two_inputs(&server, &t.org, &t.ready).await;
    let (_, created) = server
        .send(start("alice", &t.org, &t.sandbox, session(&t.ready, Value::Null)))
        .await;
    let reference = reference_of(&created["data"]);

    // Checked against the app's inputs before the provider hears of it.
    for (values, fields) in [
        (json!({}), json!({ "api_key": "Required.", "account": "Required." })),
        (
            json!({ "api_key": "  ", "account": 42 }),
            json!({ "api_key": "Required.", "account": "Must be a string." }),
        ),
        (
            json!({ "api_key": "sk_live_abcdefgh", "account": "acme" }),
            json!({ "api_key": "This doesn't look like a api key." }),
        ),
        (
            json!({ "api_key": "tk_abcdefgh", "account": "a".repeat(4097) }),
            json!({ "account": "Use 4,096 characters or fewer." }),
        ),
    ] {
        let (status, body) = server.send(submit(&reference, values.clone())).await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::BAD_REQUEST, "validation_failed"),
            "{values}"
        );
        assert_eq!(
            (&body["error"]["message"], &body["error"]["fields"]),
            (&json!("Check the highlighted fields."), &fields),
            "{values}"
        );
    }
    assert!(server.providers.sent().is_empty());

    // The provider's answer decides, and its words never come back.
    let good = json!({ "api_key": " tk_abcd1234wxyz ", "account": "acme", "extra": "dropped" });
    for (answer, status, expected) in [
        (Ok(401), StatusCode::UNPROCESSABLE_ENTITY, "credential_rejected"),
        (Ok(403), StatusCode::UNPROCESSABLE_ENTITY, "credential_rejected"),
        (Ok(404), StatusCode::BAD_GATEWAY, "provider_unavailable"),
        (Ok(500), StatusCode::BAD_GATEWAY, "provider_unavailable"),
        (
            Err(zupflow_core::outbound::OutboundError::Redirected),
            StatusCode::BAD_GATEWAY,
            "provider_unavailable",
        ),
        (
            Err(zupflow_core::outbound::OutboundError::Refused(
                zupflow_core::outbound::Refusal::NotPublic,
            )),
            StatusCode::BAD_GATEWAY,
            "provider_unavailable",
        ),
        (
            Err(zupflow_core::outbound::OutboundError::TimedOut),
            StatusCode::BAD_GATEWAY,
            "provider_unavailable",
        ),
    ] {
        server.providers.answer(answer);
        let (got, body) = server.send(submit(&reference, good.clone())).await;
        assert_eq!((got, code(&body)), (status, expected), "{answer:?}");
        assert!(!body.to_string().contains("words from the provider"));
        assert!(!body.to_string().contains("tk_abcd1234wxyz"));
    }
    let (_, rejected) = {
        server.providers.answer(Ok(401));
        server.send(submit(&reference, good.clone())).await
    };
    assert_eq!(
        (&rejected["error"]["message"], &rejected["error"]["fields"]),
        (
            &json!("tools rejected that credential. Check it and try again."),
            &json!({ "api_key": "Rejected by the provider." })
        )
    );
    let (_, hosted) = server
        .send(call("nobody", Method::GET, format!("/v1/connect/sessions/{reference}")))
        .await;
    assert_eq!(hosted["data"]["status"], "pending", "a refusal leaves the session open");
    let pool = database(&server).await;
    let (installations,): (i64,) = sqlx::query_as("SELECT count(*) FROM installations")
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert_eq!(installations, 0);

    // Accepted: one installation, its credential sealed whole, the session done.
    server.providers.answer(Ok(204));
    let (status, done) = server.send(submit(&reference, good.clone()).idempotent("k-1")).await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(
        server.providers.sent().last().unwrap(),
        &common::Sent {
            url: VERIFY.to_owned(),
            credential: Some(("X-Api-Key".to_owned(), "tk_abcd1234wxyz".to_owned())),
        },
        "the first input, trimmed, as the placement says"
    );
    let (session, installation) = (&done["data"]["session"], &done["data"]["installation"]);
    assert_eq!(
        (
            &session["status"],
            &session["installation_id"],
            &session["auth"],
            &session["expected_account"]
        ),
        (&json!("completed"), &installation["id"], &Value::Null, &Value::Null)
    );
    assert!(session["completed_at"].is_string());
    assert_eq!(
        (
            &installation["workspace_id"],
            &installation["app"]["id"],
            &installation["label"],
            &installation["status"],
            &installation["connection"]["credential_version"],
            &installation["connection"]["account"]["display_name"],
        ),
        (
            &json!(t.sandbox),
            &json!(t.ready),
            &json!("Main"),
            &json!("connected"),
            &json!(1),
            &json!("tools account ••wxyz"),
        )
    );
    assert!(!done.to_string().contains("tk_abcd1234wxyz"), "never shown");
    let installation_id = installation["id"].as_str().unwrap();
    assert_eq!(
        credential(&server, &t.org, &t.sandbox, &t.ready, installation_id).await,
        (1, json!({ "api_key": "tk_abcd1234wxyz", "account": "acme" })),
        "every declared input, nothing else"
    );

    // Once: a retry with its key replays, anything else is refused.
    let sent = server.providers.sent().len();
    assert_eq!(
        server.send(submit(&reference, good.clone()).idempotent("k-1")).await,
        (status, done.clone())
    );
    let (status, again) = server.send(submit(&reference, good.clone()).idempotent("k-2")).await;
    assert_eq!((status, code(&again)), (StatusCode::CONFLICT, "session_not_pending"));
    assert_eq!(server.providers.sent().len(), sent, "nothing more is verified");

    // The Console sees the session completed, naming its installation.
    let (_, seen) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("/v1/workspaces/{}/connect-sessions/{}", t.sandbox, id(&created["data"])),
            )
            .naming(&t.org),
        )
        .await;
    assert_eq!(
        (&seen["data"]["status"], &seen["data"]["installation_id"]),
        (&json!("completed"), &json!(installation_id))
    );
    let (_, listed) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("/v1/workspaces/{}/installations", t.sandbox),
            )
            .naming(&t.org),
        )
        .await;
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn reconnects_the_same_installation_under_the_next_version() {
    let server = Server::new().await;
    let t = tenant(&server, "alice", "northwind").await;
    let (_, created) = server
        .send(start("alice", &t.org, &t.sandbox, session(&t.ready, Value::Null)))
        .await;
    // No verify URL: the values are checked, and nothing is called.
    let (status, done) = server
        .send(submit(
            &reference_of(&created["data"]),
            json!({ "token": "first-token" }),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert!(server.providers.sent().is_empty());
    let installation = done["data"]["installation"].clone();
    let installation_id = id(&installation).to_owned();
    let (status, _) = server
        .send(
            call(
                "alice",
                Method::POST,
                format!(
                    "/v1/workspaces/{}/installations/{installation_id}/disconnect",
                    t.sandbox
                ),
            )
            .naming(&t.org),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let reconnect = json!({ "app_id": t.ready, "intent": "reconnect", "label": "ignored", "installation_id": installation_id, "return_url": null });
    let (_, renewal) = server.send(start("alice", &t.org, &t.sandbox, reconnect.clone())).await;
    let (_, racing) = server.send(start("alice", &t.org, &t.sandbox, reconnect)).await;
    let (status, renewed) = server
        .send(submit(
            &reference_of(&renewal["data"]),
            json!({ "token": "second-token" }),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{renewed}");
    let (session, after) = (&renewed["data"]["session"], &renewed["data"]["installation"]);
    assert_eq!(
        (
            &session["installation_id"],
            &after["id"],
            &after["status"],
            &after["connection"]["credential_version"],
            &after["connection"]["account"],
            &after["label"],
        ),
        (
            &json!(installation_id),
            &json!(installation_id),
            &json!("connected"),
            &json!(2),
            &installation["connection"]["account"],
            &json!("Main"),
        ),
        "the same installation and account, under the next version"
    );
    assert_eq!(
        credential(&server, &t.org, &t.sandbox, &t.ready, &installation_id).await,
        (2, json!({ "token": "second-token" }))
    );

    // Another reconnect started before it renews the same installation again.
    let (status, third) = server
        .send(submit(
            &reference_of(&racing["data"]),
            json!({ "token": "third-token" }),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{third}");
    assert_eq!(third["data"]["installation"]["connection"]["credential_version"], 3);
}

#[tokio::test]
async fn refuses_a_session_that_is_no_longer_pending_or_eligible() {
    let server = Server::new().await;
    let t = tenant(&server, "alice", "northwind").await;
    let begin = || async {
        let (status, created) = server
            .send(start("alice", &t.org, &t.sandbox, session(&t.ready, Value::Null)))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        reference_of(&created["data"])
    };
    let values = json!({ "token": "a-token" });

    let cancelled = begin().await;
    server
        .send(call(
            "nobody",
            Method::POST,
            format!("/v1/connect/sessions/{cancelled}/cancel"),
        ))
        .await;
    let (status, body) = server.send(submit(&cancelled, values.clone())).await;
    assert_eq!(
        (status, &body["error"]["message"]),
        (
            StatusCode::CONFLICT,
            &json!("This Connect session was already used. Start again from the app.")
        )
    );

    let expired = begin().await;
    let pool = database(&server).await;
    sqlx::query("UPDATE connect_sessions SET expires_at = 0 WHERE status = 'pending'")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let (status, body) = server.send(submit(&expired, values.clone())).await;
    assert_eq!(
        (status, &body["error"]["message"]),
        (
            StatusCode::CONFLICT,
            &json!("This Connect link has expired. Start again from the app.")
        )
    );

    // The app's configuration stopped being ready after the session started.
    let unready = begin().await;
    let (status, cleared) = server
        .send(
            call(
                "alice",
                Method::PUT,
                format!(
                    "/v1/organizations/{}/apps/{}/environments/sandbox/configuration",
                    t.org, t.ready
                ),
            )
            .body(json!({ "mcp_server_url": null })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    let (status, body) = server.send(submit(&unready, values.clone())).await;
    assert_eq!(
        (status, code(&body), &body["error"]["message"]),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "configuration_not_ready",
            &json!("tools isn't configured for sandbox anymore.")
        )
    );

    // An OAuth app's session takes no token.
    let (_, oauth) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{}/apps", t.org)).body(json!({
                "kind": "mcp", "name": "Docs", "slug": "docs", "description": "", "auth_type": "oauth2",
            })),
        )
        .await;
    let oauth = id(&oauth["data"]).to_owned();
    let (status, ready) = server
        .send(
            call(
                "alice",
                Method::PUT,
                format!(
                    "/v1/organizations/{}/apps/{oauth}/environments/sandbox/configuration",
                    t.org
                ),
            )
            .body(json!({
                "mcp_server_url": "https://docs.example/mcp",
                "oauth_client_id": "client-1",
                "oauth_client_secret": "a-client-secret-1234",
            })),
        )
        .await;
    assert_eq!(
        (status, &ready["data"]["status"]),
        (StatusCode::OK, &json!("ready")),
        "{ready}"
    );
    let (_, created) = server
        .send(start("alice", &t.org, &t.sandbox, session(&oauth, Value::Null)))
        .await;
    let (status, body) = server
        .send(submit(&reference_of(&created["data"]), values.clone()))
        .await;
    assert_eq!(
        (status, code(&body), &body["error"]["message"]),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "wrong_auth_type",
            &json!("Docs connects with OAuth.")
        )
    );

    // Nothing was made, and an unknown reference is the same 404 as ever.
    let (status, _) = server.send(submit("nope", values)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, listed) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("/v1/workspaces/{}/installations", t.sandbox),
            )
            .naming(&t.org),
        )
        .await;
    assert_eq!(listed["data"], json!([]));
    assert!(server.providers.sent().is_empty());
}

// --- OAuth (decision 0037) ---------------------------------------------------

const AUTHORIZE: &str = "https://accounts.example/authorize";
const TOKEN: &str = "https://accounts.example/token";
const IDENTITY: &str = "https://accounts.example/me";
const CLIENT_SECRET: &str = "not-a-real-client-secret-9f3k-abcd";

/// A ready OAuth catalog app: PKCE, an identity endpoint, `read` requested.
async fn oauth_app(server: &Server, org: &str) -> String {
    let (status, created) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/apps")).body(json!({
                "kind": "mcp", "name": "Tracker", "slug": "tracker", "description": "", "auth_type": "oauth2",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let app = id(&created["data"]).to_owned();
    let (status, body) = server
        .send(
            call("alice", Method::PATCH, format!("/v1/organizations/{org}/apps/{app}")).body(json!({ "auth": {
                "type": "oauth2", "authorize_url": AUTHORIZE, "token_url": TOKEN, "identity_url": IDENTITY,
                "available_scopes": [{ "name": "read", "description": "" }], "pkce": true,
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::PUT,
                format!("/v1/organizations/{org}/apps/{app}/environments/sandbox/configuration"),
            )
            .body(json!({
                "mcp_server_url": "https://tracker.example/mcp", "oauth_client_id": "client 1",
                "oauth_client_secret": CLIENT_SECRET, "oauth_scopes": ["read"],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    app
}

/// Starts the consent: the provider's URL and its query.
async fn authorize(server: &Server, reference: &str) -> (url::Url, std::collections::BTreeMap<String, String>) {
    let (status, body) = server
        .send(call(
            "nobody",
            Method::POST,
            format!("/v1/connect/sessions/{reference}/authorize"),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let url = url::Url::parse(body["data"]["authorization_url"].as_str().unwrap()).unwrap();
    let query = url.query_pairs().into_owned().collect();
    (url, query)
}

fn tokens(access_token: &str) -> Value {
    json!({
        "access_token": access_token, "refresh_token": "rt-1", "token_type": "Bearer",
        "expires_in": 3600, "scope": "read",
    })
}

async fn hosted(server: &Server, reference: &str) -> Value {
    let (status, body) = server
        .send(call("nobody", Method::GET, format!("/v1/connect/sessions/{reference}")))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"].clone()
}

/// Consent, then the callback: the code exchanged with the PKCE verifier
/// and the client's secret, the account identified, the tokens sealed for
/// the installation, the browser sent back to the hosted page; a `state`
/// works once, and only the newest attempt's.
#[tokio::test]
async fn connects_an_oauth_app_through_its_callback() {
    use base64::Engine;
    use sha2::Digest;
    let server = Server::new().await;
    let t = tenant(&server, "alice", "northwind").await;
    let app = oauth_app(&server, &t.org).await;
    let (_, created) = server
        .send(start("alice", &t.org, &t.sandbox, session(&app, Value::Null)))
        .await;
    let reference = reference_of(&created["data"]);

    // A token app has no consent to start.
    let (_, token_session) = server
        .send(start("alice", &t.org, &t.sandbox, session(&t.ready, Value::Null)))
        .await;
    let (status, body) = server
        .send(call(
            "nobody",
            Method::POST,
            format!(
                "/v1/connect/sessions/{}/authorize",
                reference_of(&token_session["data"])
            ),
        ))
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "wrong_auth_type")
    );

    let (url, first) = authorize(&server, &reference).await;
    assert_eq!(url.as_str().split('?').next(), Some(AUTHORIZE));
    assert_eq!(
        (
            first["response_type"].as_str(),
            first["client_id"].as_str(),
            first["redirect_uri"].as_str(),
            first["scope"].as_str(),
            first["code_challenge_method"].as_str(),
        ),
        (
            "code",
            "client 1",
            "http://localhost:8080/connect/oauth/callback",
            "read",
            "S256"
        )
    );
    assert!(!first["state"].contains(&reference), "the state is not the reference");
    let (_, second) = authorize(&server, &reference).await;
    assert_ne!(first["state"], second["state"], "a fresh state per attempt");
    assert_ne!(first["code_challenge"], second["code_challenge"]);

    // Only the newest attempt's state completes the session.
    let callback = |query: &str| format!("/connect/oauth/callback?{query}");
    for stale in [
        callback(&format!("code=c-1&state={}", first["state"])),
        callback("code=c-1&state=unknown"),
        callback("code=c-1"),
    ] {
        let (status, location, body) = server.visit(&stale).await;
        assert_eq!(
            (
                status,
                location,
                body.as_str().is_some_and(|body| body.contains("invalid_oauth_state"))
            ),
            (StatusCode::BAD_REQUEST, None, true),
            "{stale}"
        );
    }
    assert!(
        server.providers.sent().is_empty(),
        "nothing exchanged for a stale state"
    );

    server.providers.script(TOKEN, 200, tokens("at-1"));
    server.providers.script(
        IDENTITY,
        200,
        json!({ "sub": "u-42", "name": "Jordan", "email": "jordan@people.example" }),
    );
    let arrival = callback(&format!("code=c-1&state={}", second["state"]));
    let (status, location, _) = server.visit(&arrival).await;
    assert_eq!(
        (status, location.as_deref()),
        (
            StatusCode::SEE_OTHER,
            Some(format!("http://localhost:8080/connect/{reference}").as_str())
        ),
        "back to the hosted page"
    );

    // The code exchanged with the verifier, the client authenticating with Basic.
    let sent = server.providers.sent();
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("client+1:{CLIENT_SECRET}"));
    assert_eq!(
        sent.iter()
            .map(|sent| (sent.url.as_str(), sent.credential.clone()))
            .collect::<Vec<_>>(),
        [
            (TOKEN, Some(("Authorization".to_owned(), format!("Basic {basic}")))),
            (IDENTITY, Some(("Authorization".to_owned(), "Bearer at-1".to_owned()))),
        ]
    );
    let form: std::collections::BTreeMap<String, String> =
        url::form_urlencoded::parse(server.providers.bodies()[0].1.as_deref().unwrap().as_bytes())
            .into_owned()
            .collect();
    let verifier = &form["code_verifier"];
    assert_eq!(
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes())),
        second["code_challenge"],
        "the verifier of the challenge sent"
    );
    assert_eq!(
        (
            form["grant_type"].as_str(),
            form["code"].as_str(),
            form["redirect_uri"].as_str()
        ),
        (
            "authorization_code",
            "c-1",
            "http://localhost:8080/connect/oauth/callback"
        )
    );
    assert!(
        !form.contains_key("client_secret"),
        "the secret travels only as the credential"
    );

    let completed = hosted(&server, &reference).await;
    assert_eq!(completed["status"], "completed", "{completed}");
    let installation = completed["installation_id"].as_str().unwrap().to_owned();
    let (version, stored) = credential(&server, &t.org, &t.sandbox, &app, &installation).await;
    assert_eq!(
        (version, stored),
        (
            1,
            json!({ "access_token": "at-1", "refresh_token": "rt-1", "token_type": "Bearer" })
        )
    );
    let (_, read) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("/v1/workspaces/{}/installations/{installation}", t.sandbox),
            )
            .naming(&t.org),
        )
        .await;
    let connection = &read["data"]["connection"];
    assert_eq!(
        (&connection["account"], &connection["granted_scopes"]),
        (
            &json!({ "external_id": "u-42", "display_name": "Jordan", "email": "jordan@people.example" }),
            &json!(["read"])
        ),
        "{read}"
    );
    assert!(connection["expires_at"].is_string(), "{connection}");

    // The state is spent.
    let (status, _, body) = server.visit(&arrival).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.as_str().unwrap().contains("invalid_oauth_state"));
    let (status, body) = server
        .send(call(
            "nobody",
            Method::POST,
            format!("/v1/connect/sessions/{reference}/authorize"),
        ))
        .await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "session_not_pending"));

    // A reconnect with another account fails, and keeps the installation's credential.
    let (_, created) = server
        .send(start(
            "alice",
            &t.org,
            &t.sandbox,
            json!({ "app_id": app, "intent": "reconnect", "label": "Main", "installation_id": installation, "return_url": null }),
        ))
        .await;
    let reconnect = reference_of(&created["data"]);
    let (_, query) = authorize(&server, &reconnect).await;
    server.providers.script(TOKEN, 200, tokens("at-2"));
    server
        .providers
        .script(IDENTITY, 200, json!({ "id": 99, "login": "someone" }));
    let (status, _, _) = server
        .visit(&callback(&format!("code=c-2&state={}", query["state"])))
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let failed = hosted(&server, &reconnect).await;
    assert_eq!(
        (&failed["status"], &failed["failure_code"]),
        (&json!("failed"), &json!("account_mismatch"))
    );
    assert_eq!(credential(&server, &t.org, &t.sandbox, &app, &installation).await.0, 1);

    // The same account renews it under the next version.
    let (_, created) = server
        .send(start(
            "alice",
            &t.org,
            &t.sandbox,
            json!({ "app_id": app, "intent": "reconnect", "label": "Main", "installation_id": installation, "return_url": null }),
        ))
        .await;
    let reconnect = reference_of(&created["data"]);
    let (_, query) = authorize(&server, &reconnect).await;
    server.providers.script(IDENTITY, 200, json!({ "sub": "u-42" }));
    // Only scopes the session asked for or the app offers are recorded.
    server.providers.script(
        TOKEN,
        200,
        json!({ "access_token": "at-2", "token_type": "bearer", "scope": "read read admin:everything" }),
    );
    server
        .visit(&callback(&format!("code=c-3&state={}", query["state"])))
        .await;
    assert_eq!(hosted(&server, &reconnect).await["status"], "completed");
    let (version, stored) = credential(&server, &t.org, &t.sandbox, &app, &installation).await;
    assert_eq!((version, &stored["access_token"]), (2, &json!("at-2")));
    let (_, read) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("/v1/workspaces/{}/installations/{installation}", t.sandbox),
            )
            .naming(&t.org),
        )
        .await;
    assert_eq!(read["data"]["connection"]["granted_scopes"], json!(["read"]));
}

/// A refused consent, a refused code or an unreachable provider fails the
/// session with a code the hosted page explains; nothing is installed.
#[tokio::test]
async fn fails_the_session_when_the_provider_refuses() {
    let server = Server::new().await;
    let t = tenant(&server, "alice", "northwind").await;
    let app = oauth_app(&server, &t.org).await;
    let callback = |query: &str| format!("/connect/oauth/callback?{query}");
    for (outcome, failure_code) in [
        ("error=access_denied", "consent_denied"),
        ("error=server_error&error_description=words", "authorization_failed"),
        ("code=c-1&token=400", "token_exchange_failed"),
        ("code=c-1&token=503", "provider_unavailable"),
        ("code=c-1&token=200&identity=401", "identity_unavailable"),
        // A provider that echoes the client's secret: nothing of its answer is kept.
        ("code=c-1&echo=1", "token_exchange_failed"),
    ] {
        let (_, created) = server
            .send(start("alice", &t.org, &t.sandbox, session(&app, Value::Null)))
            .await;
        let reference = reference_of(&created["data"]);
        let (_, query) = authorize(&server, &reference).await;
        let script: std::collections::BTreeMap<String, String> =
            url::form_urlencoded::parse(outcome.as_bytes()).into_owned().collect();
        if let Some(status) = script.get("token") {
            server.providers.script(TOKEN, status.parse().unwrap(), tokens("at-1"));
        }
        if script.contains_key("echo") {
            server.providers.script(
                TOKEN,
                200,
                json!({ "access_token": "at-1", "token_type": "Bearer", "scope": CLIENT_SECRET }),
            );
        }
        if let Some(status) = script.get("identity") {
            server.providers.script(IDENTITY, status.parse().unwrap(), json!({}));
        }
        let (status, location, _) = server
            .visit(&callback(&format!("{outcome}&state={}", query["state"])))
            .await;
        assert_eq!(
            (status, location),
            (
                StatusCode::SEE_OTHER,
                Some(format!("http://localhost:8080/connect/{reference}"))
            ),
            "{outcome}"
        );
        let failed = hosted(&server, &reference).await;
        assert_eq!(
            (&failed["status"], &failed["failure_code"], &failed["installation_id"]),
            (&json!("failed"), &json!(failure_code), &Value::Null),
            "{outcome}"
        );
    }
    let (_, listed) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("/v1/workspaces/{}/installations", t.sandbox),
            )
            .naming(&t.org),
        )
        .await;
    assert_eq!(listed["data"], json!([]), "nothing installed");
}

/// An OAuth app whose template has no token URL yet can't start consent.
#[tokio::test]
async fn refuses_consent_without_the_providers_endpoints() {
    let server = Server::new().await;
    let t = tenant(&server, "alice", "northwind").await;
    let app = oauth_app(&server, &t.org).await;
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::PATCH,
                format!("/v1/organizations/{}/apps/{app}", t.org),
            )
            .body(json!({ "auth": {
                "type": "oauth2", "authorize_url": AUTHORIZE, "token_url": null, "identity_url": null,
                "available_scopes": [{ "name": "read", "description": "" }], "pkce": false,
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, created) = server
        .send(start("alice", &t.org, &t.sandbox, session(&app, Value::Null)))
        .await;
    let (status, body) = server
        .send(call(
            "nobody",
            Method::POST,
            format!("/v1/connect/sessions/{}/authorize", reference_of(&created["data"])),
        ))
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "configuration_not_ready")
    );
}

#[tokio::test]
async fn refuses_a_supplied_issuer_without_a_trusted_configured_issuer() {
    let server = Server::new().await;
    let t = tenant(&server, "alice", "northwind").await;
    let app = oauth_app(&server, &t.org).await;
    let (_, created) = server
        .send(start("alice", &t.org, &t.sandbox, session(&app, Value::Null)))
        .await;
    let reference = reference_of(&created["data"]);
    let (_, consent) = authorize(&server, &reference).await;
    let (status, _, _) = server
        .visit(&format!(
            "/connect/oauth/callback?code=c&state={}&iss=https%3A%2F%2Faccounts.example",
            consent["state"]
        ))
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(
        server.providers.sent().is_empty(),
        "an unbound issuer exchanges nothing"
    );
    let (_, body) = server
        .send(call("nobody", Method::GET, format!("/v1/connect/sessions/{reference}")))
        .await;
    assert_eq!(body["data"]["failure_code"], "authorization_failed");
}
