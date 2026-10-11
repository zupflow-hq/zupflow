//! Sign-in through the MCP server end to end (decision 0039): each start
//! discovers the server's authorization server; a client is registered there
//! once per catalog app and environment, or per workspace app; consent and
//! the exchange use it with PKCE and the server as the `resource`; and the
//! installation keeps where it signed in.

#![allow(clippy::unwrap_used)]

mod common;

use std::collections::BTreeMap;

use axum::http::{Method, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use common::{Server, call, code};
use serde_json::{Value, json};
use sha2::Digest;
use zupflow_core::ids::{InstallationId, OrganizationId, WorkspaceId};
use zupflow_server::discovery::{Outcome, discover};

const MCP: &str = "https://ledger.example/mcp";
const RESOURCE_METADATA: &str = "https://ledger.example/.well-known/oauth-protected-resource/mcp";
const ISSUER: &str = "https://auth.example/ledger";
const AS_METADATA: &str = "https://auth.example/.well-known/oauth-authorization-server/ledger";
const AUTHORIZE: &str = "https://auth.example/ledger/authorize";
const TOKEN: &str = "https://auth.example/ledger/token";
const REGISTER: &str = "https://auth.example/ledger/register";
const CALLBACK: &str = "http://localhost:8080/connect/oauth/callback";

fn id(value: &Value) -> &str {
    value["id"].as_str().unwrap()
}

struct Tenant {
    org: String,
    sandbox: String,
    other: String,
}

async fn tenant(server: &Server) -> Tenant {
    let org = server.organization("alice", "northwind").await;
    let sandbox = server.workspace("alice", &org, "acme-sandbox", "sandbox").await;
    let other = server.workspace("alice", &org, "beta-sandbox", "sandbox").await;
    Tenant {
        org,
        sandbox: id(&sandbox).to_owned(),
        other: id(&other).to_owned(),
    }
}

fn template(identity_url: Option<&str>) -> Value {
    json!({ "type": "mcp_oauth", "scopes": [{ "name": "read", "description": "Read entries" }], "identity_url": identity_url })
}

/// A catalog `mcp_oauth` app served at [`MCP`] in Sandbox.
async fn catalog_app(server: &Server, org: &str, identity_url: Option<&str>) -> String {
    let (status, created) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/apps")).body(json!({
                "kind": "mcp", "name": "Ledger", "slug": "ledger", "description": "", "auth_type": "mcp_oauth",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(
        created["data"]["auth"],
        json!({ "type": "mcp_oauth", "scopes": [], "identity_url": null })
    );
    let app = id(&created["data"]).to_owned();
    let (status, body) = server
        .send(
            call("alice", Method::PATCH, format!("/v1/organizations/{org}/apps/{app}"))
                .body(json!({ "auth": template(identity_url) })),
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
            .body(json!({ "mcp_server_url": MCP })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        (&body["data"]["status"], &body["data"]["oauth"]),
        (&json!("ready"), &Value::Null)
    );
    app
}

/// The server and its authorization server, as discovery finds them, and a
/// registration endpoint that issues `registered`.
fn serve(server: &Server, registered: Value) {
    let providers = &server.providers;
    providers.challenge(
        MCP,
        &format!(r#"Bearer error="invalid_token", resource_metadata="{RESOURCE_METADATA}""#),
    );
    providers.script(
        RESOURCE_METADATA,
        200,
        json!({ "resource": MCP, "authorization_servers": [ISSUER] }),
    );
    providers.script(AS_METADATA, 200, metadata(ISSUER));
    providers.script(REGISTER, 201, registered);
    providers.serve_mcp(
        MCP,
        json!([{ "name": "list_entries", "description": "Lists entries.", "inputSchema": { "type": "object" }, "annotations": { "readOnlyHint": true } }]),
    );
}

fn metadata(issuer: &str) -> Value {
    json!({
        "issuer": issuer,
        "authorization_endpoint": AUTHORIZE,
        "token_endpoint": TOKEN,
        "registration_endpoint": REGISTER,
        "code_challenge_methods_supported": ["S256"],
    })
}

async fn start(server: &Server, t: &Tenant, ws: &str, app: &str, installation: Option<&str>) -> String {
    let (status, created) = server
        .send(
            call("alice", Method::POST, format!("/v1/workspaces/{ws}/connect-sessions"))
                .naming(&t.org)
                .body(json!({
                    "app_id": app,
                    "intent": if installation.is_some() { "reconnect" } else { "create" },
                    "label": "Books",
                    "installation_id": installation,
                    "return_url": null,
                })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    created["data"]["url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned()
}

async fn authorize(server: &Server, reference: &str) -> (StatusCode, Value) {
    server
        .send(call(
            "nobody",
            Method::POST,
            format!("/v1/connect/sessions/{reference}/authorize"),
        ))
        .await
}

/// Starts the consent: the authorization server's URL and its query.
async fn consent(server: &Server, reference: &str) -> (String, BTreeMap<String, String>) {
    let (status, body) = authorize(server, reference).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let url = url::Url::parse(body["data"]["authorization_url"].as_str().unwrap()).unwrap();
    let query = url.query_pairs().into_owned().collect();
    (url.as_str().split('?').next().unwrap().to_owned(), query)
}

async fn hosted(server: &Server, reference: &str) -> Value {
    let (status, body) = server
        .send(call("nobody", Method::GET, format!("/v1/connect/sessions/{reference}")))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"].clone()
}

async fn arrive(server: &Server, code: &str, state: &str) {
    let (status, _, _) = server
        .visit(&format!("/connect/oauth/callback?code={code}&state={state}"))
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
}

/// An access token that is a JWT naming `sub`.
fn jwt(sub: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256"}"#);
    let claims = URL_SAFE_NO_PAD.encode(json!({ "sub": sub, "aud": MCP }).to_string());
    format!("{header}.{claims}.signature")
}

/// The form a request sent to `url` carried, the last time.
fn form(server: &Server, url: &str) -> BTreeMap<String, String> {
    let bodies = server.providers.bodies();
    let (_, body) = bodies.iter().rev().find(|(sent, _)| sent == url).unwrap();
    url::form_urlencoded::parse(body.as_deref().unwrap().as_bytes())
        .into_owned()
        .collect()
}

async fn database(server: &Server) -> sqlx::SqlitePool {
    sqlx::SqlitePool::connect(&format!("sqlite:{}", server.directory.join("zupflow.sqlite").display()))
        .await
        .unwrap()
}

async fn installation(server: &Server, t: &Tenant, ws: &str, installation: &str) -> Value {
    let (status, read) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("/v1/workspaces/{ws}/installations/{installation}"),
            )
            .naming(&t.org),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{read}");
    read["data"].clone()
}

#[tokio::test]
async fn connects_through_the_servers_own_sign_in() {
    let server = Server::new().await;
    let t = tenant(&server).await;
    let app = catalog_app(&server, &t.org, None).await;
    serve(&server, json!({ "client_id": "client-1", "redirect_uris": [CALLBACK] }));
    let reference = start(&server, &t, &t.sandbox, &app, None).await;

    // Discovery, then one registration: a public client for the one callback.
    let (url, query) = consent(&server, &reference).await;
    assert_eq!(url, AUTHORIZE);
    assert_eq!(
        server.providers.urls(),
        [MCP, RESOURCE_METADATA, AS_METADATA, REGISTER],
        "challenged, read the metadata, registered"
    );
    assert!(
        server.providers.sent().iter().all(|sent| sent.credential.is_none()),
        "nothing authenticated"
    );
    let registration: Value = serde_json::from_str(server.providers.bodies()[3].1.as_deref().unwrap()).unwrap();
    assert_eq!(
        registration,
        json!({
            "client_name": "Zupflow",
            "redirect_uris": [CALLBACK],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        })
    );
    assert_eq!(
        (
            query["response_type"].as_str(),
            query["client_id"].as_str(),
            query["redirect_uri"].as_str(),
            query["scope"].as_str(),
            query["code_challenge_method"].as_str(),
            query["resource"].as_str(),
        ),
        ("code", "client-1", CALLBACK, "read", "S256", MCP)
    );

    // A second start discovers again, and uses the client registered.
    let (_, query) = consent(&server, &reference).await;
    assert_eq!(query["client_id"], "client-1");
    let registrations = server.providers.urls().iter().filter(|url| *url == REGISTER).count();
    assert_eq!(registrations, 1, "registered once");
    assert_eq!(
        server.providers.urls().iter().filter(|url| *url == AS_METADATA).count(),
        2,
        "nothing cached across starts"
    );

    // The exchange: a public client naming itself, the verifier, the resource.
    server.providers.script(
        TOKEN,
        200,
        json!({ "access_token": jwt("u-7"), "token_type": "Bearer", "refresh_token": "rt-1", "expires_in": 3600, "scope": "read write" }),
    );
    arrive(&server, "c-1", &query["state"]).await;
    let exchanged = form(&server, TOKEN);
    assert_eq!(
        (
            exchanged["grant_type"].as_str(),
            exchanged["code"].as_str(),
            exchanged["redirect_uri"].as_str(),
            exchanged["client_id"].as_str(),
            exchanged["resource"].as_str(),
        ),
        ("authorization_code", "c-1", CALLBACK, "client-1", MCP)
    );
    assert_eq!(
        URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(exchanged["code_verifier"].as_bytes())),
        query["code_challenge"],
        "the verifier of the challenge sent"
    );
    let token_request = server
        .providers
        .sent()
        .into_iter()
        .rev()
        .find(|sent| sent.url == TOKEN)
        .unwrap();
    assert_eq!(token_request.credential, None, "a public client sends no secret");

    // Completed, as an anonymous account named from the issuer and the token's subject.
    let completed = hosted(&server, &reference).await;
    assert_eq!(completed["status"], "completed", "{completed}");
    let installed = completed["installation_id"].as_str().unwrap().to_owned();
    let read = installation(&server, &t, &t.sandbox, &installed).await;
    let derived: String = sha2::Sha256::digest(format!("{ISSUER}\nu-7").as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(
        (
            &read["app"]["auth_type"],
            &read["connection"]["account"],
            &read["connection"]["granted_scopes"]
        ),
        (
            &json!("mcp_oauth"),
            &json!({ "external_id": derived, "display_name": "Ledger account", "email": null }),
            &json!(["read"])
        ),
        "{read}"
    );

    // The installation keeps where it signed in; the organization keeps one client.
    let pool = database(&server).await;
    let kept: String = sqlx::query_scalar("SELECT oauth_client FROM installations WHERE id = ?")
        .bind(InstallationId::parse(&installed).unwrap().uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&kept).unwrap(),
        json!({ "issuer": ISSUER, "response_iss_supported": false, "token_endpoint": TOKEN, "client_id": "client-1", "resource": MCP })
    );
    let clients: Vec<(String, String, Option<Vec<u8>>)> = sqlx::query_as(
        "SELECT client_id, token_endpoint_auth_method, ciphertext FROM oauth_client_registrations \
         WHERE organization_id = ?",
    )
    .bind(OrganizationId::parse(&t.org).unwrap().uuid())
    .fetch_all(&pool)
    .await
    .unwrap();
    pool.close().await;
    assert_eq!(clients, [("client-1".to_owned(), "none".to_owned(), None)]);

    // Its tools are discovered with the access token, as an OAuth app's are.
    let ids = (
        OrganizationId::parse(&t.org).unwrap(),
        WorkspaceId::parse(&t.sandbox).unwrap(),
        InstallationId::parse(&installed).unwrap(),
    );
    assert_eq!(discover(&server.state, ids.0, ids.1, ids.2).await, Outcome::Done);
    let listed = server
        .providers
        .sent()
        .into_iter()
        .rev()
        .find(|sent| sent.url == MCP)
        .unwrap();
    assert_eq!(
        listed.credential,
        Some(("Authorization".to_owned(), format!("Bearer {}", jwt("u-7"))))
    );
    let tools = server
        .state
        .tools
        .list_tools(ids.0, ids.1, ids.2, None, 10)
        .await
        .unwrap()
        .items;
    assert_eq!(
        tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(),
        ["list_entries"]
    );

    // A reconnect whose token names another subject is another account:
    // it fails, and the credential stays as it was.
    let reconnect = start(&server, &t, &t.sandbox, &app, Some(&installed)).await;
    let (_, query) = consent(&server, &reconnect).await;
    server.providers.script(
        TOKEN,
        200,
        json!({ "access_token": jwt("someone-else"), "token_type": "Bearer" }),
    );
    arrive(&server, "c-2", &query["state"]).await;
    assert_eq!(hosted(&server, &reconnect).await["failure_code"], "account_mismatch");
    let kept = installation(&server, &t, &t.sandbox, &installed).await;
    assert_eq!(kept["connection"], read["connection"]);

    // The same subject reconnects.
    let reconnect = start(&server, &t, &t.sandbox, &app, Some(&installed)).await;
    let (_, query) = consent(&server, &reconnect).await;
    server.providers.script(
        TOKEN,
        200,
        json!({ "access_token": jwt("u-7"), "token_type": "Bearer" }),
    );
    arrive(&server, "c-3", &query["state"]).await;
    assert_eq!(hosted(&server, &reconnect).await["status"], "completed");
    let again = installation(&server, &t, &t.sandbox, &installed).await;
    assert_eq!(again["connection"]["account"], read["connection"]["account"]);
    assert_eq!(again["connection"]["credential_version"], 2);

    // A token that names no subject can't be checked: the account it had is kept.
    let reconnect = start(&server, &t, &t.sandbox, &app, Some(&installed)).await;
    let (_, query) = consent(&server, &reconnect).await;
    server.providers.script(
        TOKEN,
        200,
        json!({ "access_token": "opaque-1", "token_type": "Bearer" }),
    );
    arrive(&server, "c-4", &query["state"]).await;
    assert_eq!(hosted(&server, &reconnect).await["status"], "completed");
    let again = installation(&server, &t, &t.sandbox, &installed).await;
    assert_eq!(again["connection"]["account"], read["connection"]["account"]);
    assert_eq!(again["connection"]["credential_version"], 3);

    // After the server moved to another authorization server, a subject names
    // an account there, not the one before: the reconnect can't be checked
    // and keeps the account it had, whatever the subject.
    let moved = "https://auth.example/moved";
    server.providers.script(
        RESOURCE_METADATA,
        200,
        json!({ "resource": MCP, "authorization_servers": [moved] }),
    );
    server.providers.script(
        "https://auth.example/.well-known/oauth-authorization-server/moved",
        200,
        metadata(moved),
    );
    server
        .providers
        .script(REGISTER, 201, json!({ "client_id": "moved-client" }));
    let reconnect = start(&server, &t, &t.sandbox, &app, Some(&installed)).await;
    let (_, query) = consent(&server, &reconnect).await;
    server.providers.script(
        TOKEN,
        200,
        json!({ "access_token": jwt("someone-else"), "token_type": "Bearer" }),
    );
    arrive(&server, "c-5", &query["state"]).await;
    assert_eq!(hosted(&server, &reconnect).await["status"], "completed");
    let again = installation(&server, &t, &t.sandbox, &installed).await;
    assert_eq!(again["connection"]["account"], read["connection"]["account"]);
    assert_eq!(again["connection"]["credential_version"], 4);

    // The token was asked for this server: never sent to another the configuration names since.
    let elsewhere = "https://elsewhere.example/mcp";
    let (status, _) = server
        .send(
            call(
                "alice",
                Method::PUT,
                format!(
                    "/v1/organizations/{}/apps/{app}/environments/sandbox/configuration",
                    t.org
                ),
            )
            .body(json!({ "mcp_server_url": elsewhere })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(discover(&server.state, ids.0, ids.1, ids.2).await, Outcome::Done);
    assert!(!server.providers.urls().iter().any(|url| url == elsewhere));
}

#[tokio::test]
async fn callback_errors_leave_a_shared_registration_usable() {
    let server = Server::new().await;
    let t = tenant(&server).await;
    let app = catalog_app(&server, &t.org, None).await;
    serve(&server, json!({ "client_id": "client-1", "redirect_uris": [CALLBACK] }));
    let mut advertised = metadata(ISSUER);
    advertised["authorization_response_iss_parameter_supported"] = json!(true);
    server.providers.script(AS_METADATA, 200, advertised);

    let reference = start(&server, &t, &t.sandbox, &app, None).await;
    let (_, query) = consent(&server, &reference).await;
    let sibling = start(&server, &t, &t.other, &app, None).await;
    let (_, sibling_query) = consent(&server, &sibling).await;
    assert_eq!(query["client_id"], sibling_query["client_id"]);

    // Knowing the expected issuer doesn't authenticate a browser query.
    let callback = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("state", &query["state"])
        .append_pair("iss", ISSUER)
        .append_pair("error", "invalid_client")
        .finish();
    let (status, _, _) = server.visit(&format!("/connect/oauth/callback?{callback}")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let failed = hosted(&server, &reference).await;
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["failure_code"], "authorization_failed");
    assert!(server.providers.requests_to(TOKEN).is_empty());

    // The other workspace's already-started exchange still uses that registration.
    server.providers.script(
        TOKEN,
        200,
        json!({ "access_token": jwt("u-7"), "token_type": "Bearer", "refresh_token": "rt-1", "expires_in": 3600 }),
    );
    let callback = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("state", &sibling_query["state"])
        .append_pair("iss", ISSUER)
        .append_pair("code", "c-1")
        .finish();
    let (status, _, _) = server.visit(&format!("/connect/oauth/callback?{callback}")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(hosted(&server, &sibling).await["status"], "completed");
    assert_eq!(form(&server, TOKEN)["client_id"], "client-1");

    let again = start(&server, &t, &t.other, &app, None).await;
    assert_eq!(consent(&server, &again).await.1["client_id"], "client-1");
    assert_eq!(server.providers.requests_to(REGISTER).len(), 1);
}

/// A server that issues a secret gets Basic, and an answer that echoes the
/// secret is refused; an identity endpoint names the account and holds a
/// reconnect to it.
#[tokio::test]
async fn authenticates_as_registered_and_identifies_the_account() {
    const IDENTITY: &str = "https://ledger.example/me";
    let server = Server::new().await;
    let t = tenant(&server).await;
    let app = catalog_app(&server, &t.org, Some(IDENTITY)).await;
    serve(
        &server,
        json!({ "client_id": "client-9", "client_secret": "issued/secret-1", "registration_access_token": "rat-1" }),
    );

    // The secret travels only as Basic, and an answer carrying it back is refused.
    let reference = start(&server, &t, &t.sandbox, &app, None).await;
    let (_, query) = consent(&server, &reference).await;
    server.providers.script(
        TOKEN,
        200,
        json!({ "access_token": "at-1", "token_type": "Bearer", "debug": "issued/secret-1" }),
    );
    arrive(&server, "c-1", &query["state"]).await;
    let failed = hosted(&server, &reference).await;
    assert_eq!(
        (&failed["status"], &failed["failure_code"]),
        (&json!("failed"), &json!("token_exchange_failed"))
    );
    let token_request = server
        .providers
        .sent()
        .into_iter()
        .rev()
        .find(|sent| sent.url == TOKEN)
        .unwrap();
    let basic = STANDARD.encode("client-9:issued%2Fsecret-1");
    assert_eq!(
        token_request.credential,
        Some(("Authorization".to_owned(), format!("Basic {basic}")))
    );
    let exchanged = form(&server, TOKEN);
    assert!(!exchanged.contains_key("client_id") && !exchanged.contains_key("client_secret"));
    assert_eq!(exchanged["resource"], MCP);

    // What was issued is sealed, never stored as it came.
    let pool = database(&server).await;
    let (method, ciphertext): (String, Vec<u8>) = sqlx::query_as(
        "SELECT token_endpoint_auth_method, ciphertext FROM oauth_client_registrations WHERE organization_id = ?",
    )
    .bind(OrganizationId::parse(&t.org).unwrap().uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    assert_eq!(method, "client_secret_basic");
    for issued in [&b"issued/secret-1"[..], b"rat-1"] {
        assert!(!ciphertext.windows(issued.len()).any(|window| window == issued));
    }

    // The identity endpoint names the account.
    let reference = start(&server, &t, &t.sandbox, &app, None).await;
    let (_, query) = consent(&server, &reference).await;
    server
        .providers
        .script(TOKEN, 200, json!({ "access_token": "at-2", "token_type": "Bearer" }));
    server.providers.script(
        IDENTITY,
        200,
        json!({ "sub": "u-1", "name": "Jordan", "email": "jordan@people.example" }),
    );
    arrive(&server, "c-2", &query["state"]).await;
    let completed = hosted(&server, &reference).await;
    assert_eq!(completed["status"], "completed", "{completed}");
    let installed = completed["installation_id"].as_str().unwrap().to_owned();
    let read = installation(&server, &t, &t.sandbox, &installed).await;
    assert_eq!(read["connection"]["account"]["external_id"], "u-1");

    // A reconnect with another account fails, as an OAuth app's does.
    let reconnect = start(&server, &t, &t.sandbox, &app, Some(&installed)).await;
    let (_, query) = consent(&server, &reconnect).await;
    server
        .providers
        .script(TOKEN, 200, json!({ "access_token": "at-3", "token_type": "Bearer" }));
    server.providers.script(IDENTITY, 200, json!({ "sub": "u-2" }));
    arrive(&server, "c-3", &query["state"]).await;
    assert_eq!(hosted(&server, &reconnect).await["failure_code"], "account_mismatch");

    // The identity URL must stay on the server: one the configuration has
    // moved away from fails sign-in before anything is sent.
    let (status, _) = server
        .send(
            call(
                "alice",
                Method::PUT,
                format!(
                    "/v1/organizations/{}/apps/{app}/environments/sandbox/configuration",
                    t.org
                ),
            )
            .body(json!({ "mcp_server_url": "https://moved.example/mcp" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let sent = server.providers.urls().len();
    let reference = start(&server, &t, &t.sandbox, &app, None).await;
    let (status, body) = authorize(&server, &reference).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "identity_off_server")
    );
    assert_eq!(hosted(&server, &reference).await["failure_code"], "identity_off_server");
    assert_eq!(server.providers.urls().len(), sent, "nothing sent");

    // And a template can't name one off a configured server.
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::PATCH,
                format!("/v1/organizations/{}/apps/{app}", t.org),
            )
            .body(json!({ "auth": template(Some(IDENTITY)) })),
        )
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "identity_off_server")
    );
    assert_eq!(
        body["error"]["fields"]["auth.identity_url"],
        "Use an address on https://moved.example, the Sandbox configuration's MCP server."
    );
}

/// One client per catalog app and environment, shared by its workspaces; a
/// workspace app's own in each workspace; another issuer, another client.
#[tokio::test]
async fn registers_once_per_app_environment_and_issuer() {
    let server = Server::new().await;
    let t = tenant(&server).await;
    let app = catalog_app(&server, &t.org, None).await;
    serve(&server, json!({ "client_id": "catalog-client" }));
    for ws in [&t.sandbox, &t.other] {
        let reference = start(&server, &t, ws, &app, None).await;
        assert_eq!(consent(&server, &reference).await.1["client_id"], "catalog-client");
    }
    let registered = || server.providers.urls().iter().filter(|url| *url == REGISTER).count();
    assert_eq!(registered(), 1, "shared by the organization's workspaces");

    let mut own = Vec::new();
    for (ws, client) in [(&t.sandbox, "own-1"), (&t.other, "own-2")] {
        let (status, created) = server
            .send(
                call("alice", Method::POST, format!("/v1/workspaces/{ws}/apps"))
                    .naming(&t.org)
                    .body(json!({ "kind": "mcp", "name": "Ledger", "slug": "ledger", "description": "", "auth_type": "mcp_oauth" })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        let app = id(&created["data"]).to_owned();
        let (status, _) = server
            .send(
                call(
                    "alice",
                    Method::PUT,
                    format!("/v1/workspaces/{ws}/apps/{app}/configuration"),
                )
                .naming(&t.org)
                .body(json!({ "mcp_server_url": MCP })),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        server.providers.script(REGISTER, 201, json!({ "client_id": client }));
        let reference = start(&server, &t, ws, &app, None).await;
        assert_eq!(consent(&server, &reference).await.1["client_id"], client);
        own.push(app);
    }
    assert_eq!(registered(), 3, "each workspace app its own");

    // The server moved to another authorization server: a new client there,
    // and the first kept.
    let moved = "https://auth.example/moved";
    server.providers.script(
        RESOURCE_METADATA,
        200,
        json!({ "resource": MCP, "authorization_servers": [moved] }),
    );
    server.providers.script(
        "https://auth.example/.well-known/oauth-authorization-server/moved",
        200,
        metadata(moved),
    );
    server
        .providers
        .script(REGISTER, 201, json!({ "client_id": "moved-client" }));
    let reference = start(&server, &t, &t.sandbox, &app, None).await;
    assert_eq!(consent(&server, &reference).await.1["client_id"], "moved-client");
    let pool = database(&server).await;
    let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM oauth_client_registrations WHERE organization_id = ?")
        .bind(OrganizationId::parse(&t.org).unwrap().uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    let workspace: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workspace_oauth_client_registrations WHERE organization_id = ? AND workspace_id = ?",
    )
    .bind(OrganizationId::parse(&t.org).unwrap().uuid())
    .bind(WorkspaceId::parse(&t.sandbox).unwrap().uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    assert_eq!((kept, workspace), (2, 1));
}

/// A server whose sign-in can't be used fails the session with
/// `discovery_failed`; one that can't be reached, with `provider_unavailable`.
#[tokio::test]
async fn fails_the_session_when_discovery_fails() {
    let server = Server::new().await;
    let t = tenant(&server).await;
    let app = catalog_app(&server, &t.org, None).await;
    serve(&server, json!({ "client_id": "client-1" }));

    for (change, expected) in [
        (
            json!({ "issuer": "https://auth.example/elsewhere" }),
            "discovery_failed",
        ),
        (
            json!({ "code_challenge_methods_supported": ["plain"] }),
            "discovery_failed",
        ),
        (json!({ "registration_endpoint": null }), "discovery_failed"),
        // The browser would go there: a private host is refused, as in a template.
        (
            json!({ "authorization_endpoint": "https://10.0.0.7/ledger/authorize" }),
            "discovery_failed",
        ),
        (
            json!({ "token_endpoint": "http://auth.example/ledger/token" }),
            "discovery_failed",
        ),
    ] {
        let mut changed = metadata(ISSUER);
        for (key, value) in change.as_object().unwrap() {
            changed[key] = value.clone();
        }
        server.providers.script(AS_METADATA, 200, changed);
        let reference = start(&server, &t, &t.sandbox, &app, None).await;
        let (status, body) = authorize(&server, &reference).await;
        assert_eq!((status, code(&body)), (StatusCode::BAD_GATEWAY, expected), "{change}");
        let failed = hosted(&server, &reference).await;
        assert_eq!(
            (&failed["status"], &failed["failure_code"]),
            (&json!("failed"), &json!(expected))
        );
    }

    // A registration the server refuses can't be used either.
    server.providers.script(AS_METADATA, 200, metadata(ISSUER));
    server
        .providers
        .script(REGISTER, 400, json!({ "error": "words from the server" }));
    let reference = start(&server, &t, &t.sandbox, &app, None).await;
    let (status, body) = authorize(&server, &reference).await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_GATEWAY, "discovery_failed"));
    assert!(!body.to_string().contains("words from the server"));

    // Down: unreachable, or answering 5xx.
    server.providers.script(REGISTER, 503, Value::Null);
    let reference = start(&server, &t, &t.sandbox, &app, None).await;
    let (status, body) = authorize(&server, &reference).await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_GATEWAY, "provider_unavailable"));
    assert_eq!(
        hosted(&server, &reference).await["failure_code"],
        "provider_unavailable"
    );
    server.providers.challenges.lock().unwrap().clear();
    server.providers.mcp.lock().unwrap().clear();
    server.providers.scripted.lock().unwrap().clear();
    server
        .providers
        .answer(Err(zupflow_core::outbound::OutboundError::Unreachable));
    let reference = start(&server, &t, &t.sandbox, &app, None).await;
    let (status, body) = authorize(&server, &reference).await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_GATEWAY, "provider_unavailable"));
    assert_eq!(
        hosted(&server, &reference).await["failure_code"],
        "provider_unavailable"
    );
}

/// No OAuth client to configure: the configuration refuses one, and a
/// session asks for the template's scopes.
#[tokio::test]
async fn configures_nothing_but_the_server() {
    let server = Server::new().await;
    let t = tenant(&server).await;
    let app = catalog_app(&server, &t.org, None).await;
    for body in [
        json!({ "oauth_client_id": "client" }),
        json!({ "oauth_client_secret": "a-long-client-secret" }),
        json!({ "oauth_scopes": ["read"] }),
    ] {
        let (status, answer) = server
            .send(
                call(
                    "alice",
                    Method::PUT,
                    format!(
                        "/v1/organizations/{}/apps/{app}/environments/sandbox/configuration",
                        t.org
                    ),
                )
                .body(body.clone()),
            )
            .await;
        assert_eq!(
            (status, code(&answer)),
            (StatusCode::UNPROCESSABLE_ENTITY, "oauth_not_supported"),
            "{body}"
        );
    }
    let reference = start(&server, &t, &t.sandbox, &app, None).await;
    assert_eq!(hosted(&server, &reference).await["requested_scopes"], json!(["read"]));
}
