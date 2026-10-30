//! The HTTP surface, end to end over a real database: health, the envelope
//! for paths and methods the API doesn't serve, and the Console's identity,
//! organization and member routes, with their isolation and role rules.

#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use zupflow_core::identity::{Identity, IdentityError, Profile, VerifiedIdentity};
use zupflow_server::{AppState, RequestGuard};
use zupflow_sqlite::{Database, SqliteApiKeyStore, SqliteOrganizationStore, SqliteWorkspaceStore};

/// Signs in whoever a `Bearer test:{name}` token names, as `{name}@people.example`.
struct TestIdentity;

#[async_trait]
impl Identity for TestIdentity {
    async fn verify(&self, bearer: Option<&str>) -> Result<VerifiedIdentity, IdentityError> {
        let name = bearer
            .and_then(|token| token.strip_prefix("test:"))
            .ok_or(IdentityError::Unauthenticated)?;
        Ok(VerifiedIdentity {
            provider: "test".to_owned(),
            subject: name.to_owned(),
            profile: Profile {
                name: name.to_owned(),
                email: format!("{name}@people.example"),
                email_verified: true,
                avatar_url: None,
            },
        })
    }
}

struct Server {
    app: axum::Router,
    directory: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

impl Server {
    async fn new() -> Self {
        Self::with_guard(RequestGuard::default()).await
    }

    async fn with_guard(guard: RequestGuard) -> Self {
        let directory = std::env::temp_dir().join(format!("zupflow-http-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir(&directory).unwrap();
        let database = Database::open(&directory.join("zupflow.sqlite")).await.unwrap();
        let state = AppState {
            clock: Arc::new(zupflow_core::time::SystemClock),
            identity: Arc::new(TestIdentity),
            organizations: Arc::new(SqliteOrganizationStore::new(database.clone())),
            workspaces: Arc::new(SqliteWorkspaceStore::new(database.clone())),
            apps: Arc::new(zupflow_sqlite::SqliteAppStore::new(database.clone())),
            installations: Arc::new(zupflow_sqlite::SqliteInstallationStore::new(database.clone())),
            agents: Arc::new(zupflow_sqlite::SqliteAgentStore::new(database.clone())),
            policies: Arc::new(zupflow_sqlite::SqliteToolPolicyStore::new(database.clone())),
            runs: Arc::new(zupflow_sqlite::SqliteRunStore::new(database.clone())),
            provider: Arc::new(zupflow_basic::scripted_model::ScriptedModel::default()),
            approvals: Arc::new(zupflow_basic::configured_policy::ConfiguredPolicy::new(Arc::new(
                zupflow_sqlite::SqliteToolPolicyStore::new(database.clone()),
            ))),
            events: Arc::new(zupflow_basic::in_process_events::InProcessEventBus::default()),
            metrics: Arc::new(zupflow_basic::in_process_metrics::InProcessMetrics::default()),
            model_store: Arc::new(zupflow_sqlite::SqliteModelStore::new(database.clone())),
            model_catalog: Arc::new(zupflow_basic::no_providers::NoProviders),
            memory_types: Arc::new(zupflow_sqlite::SqliteMemoryTypeStore::new(database.clone())),
            memory_providers: zupflow_server::bootstrap::memory_providers(Arc::new(
                zupflow_sqlite::SqliteNoteStore::new(database.clone()),
            )),
            connect: Arc::new(zupflow_sqlite::SqliteConnectStore::new(database.clone())),
            channels: Arc::new(zupflow_sqlite::SqliteChannelStore::new(database.clone())),
            channel_providers: zupflow_core::channels::registry::ChannelProviders::none(),
            jobs: Arc::new(zupflow_sqlite::SqliteJobQueue::new(database.clone())),
            tools: Arc::new(zupflow_sqlite::SqliteToolRegistry::new(database.clone())),
            cache: Arc::new(zupflow_basic::memory_cache::MemoryCache::default()),
            keys: Arc::new(zupflow_basic::local_keys::LocalKeyService::from_server_secret(&[7; 32])),
            outbound: Arc::new(zupflow_outbound::SafeHttpClient::new().unwrap()),
            public_url: "http://localhost:8080".to_owned(),
            sign_in: zupflow_server::sign_in::SignInSettings::Local,
            api_keys: Arc::new(SqliteApiKeyStore::new(database.clone())),
            webhooks: Arc::new(zupflow_sqlite::SqliteWebhookStore::new(database.clone())),
            audit: Arc::new(zupflow_sqlite::SqliteAuditStore::new(database.clone())),
            idempotency: Arc::new(zupflow_sqlite::SqliteIdempotencyStore::new(database)),
            fingerprinter: Arc::new(zupflow_core::idempotency::Fingerprinter::new([7; 32])),
            api_key_hasher: Arc::new(zupflow_core::api_keys::ApiKeyHasher::new([7; 32])),
        };
        Self {
            app: zupflow_server::app(state, guard),
            directory,
        }
    }

    async fn call(&self, as_user: Option<&str>, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(user) = as_user {
            request = request.header("Authorization", format!("Bearer test:{user}"));
        }
        let request = match body {
            Some(body) => request
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        }
        .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, json)
    }

    /// A raw request with exactly these headers, for the guard's checks.
    async fn raw(&self, method: Method, uri: &str, headers: &[(&str, &str)], body: &str) -> (StatusCode, Value) {
        let (status, _, json) = self.exchange(method, uri, headers, body).await;
        (status, json)
    }

    /// A raw request, answered with the response's headers too.
    async fn exchange(
        &self,
        method: Method,
        uri: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (StatusCode, axum::http::HeaderMap, Value) {
        let mut request = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, headers, json)
    }

    async fn get(&self, user: &str, uri: &str) -> (StatusCode, Value) {
        self.call(Some(user), Method::GET, uri, None).await
    }

    async fn send(&self, user: &str, method: Method, uri: &str, body: Value) -> (StatusCode, Value) {
        self.call(Some(user), method, uri, Some(body)).await
    }

    /// Creates an organization owned by `user`; its ID.
    async fn organization(&self, user: &str, slug: &str) -> String {
        let (status, body) = self
            .send(
                user,
                Method::POST,
                "/v1/organizations",
                json!({ "name": "Test", "slug": slug }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["data"]["id"].as_str().unwrap().to_owned()
    }

    /// Invites `user` into `org` with `role`, and accepts as them.
    async fn member(&self, owner: &str, org: &str, user: &str, role: &str) -> String {
        // Sign them in once, so their user exists with their identity.
        self.get(user, "/v1/me").await;
        let (status, body) = self
            .send(
                owner,
                Method::POST,
                &format!("/v1/organizations/{org}/members"),
                json!({ "email": format!("{user}@people.example"), "role": role }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let id = body["data"]["id"].as_str().unwrap();
        let (status, joined) = self
            .call(
                Some(user),
                Method::POST,
                &format!("/v1/me/invitations/{id}/accept"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{joined}");
        body["data"]["id"].as_str().unwrap().to_owned()
    }
}

fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn reports_health_and_readiness() {
    let server = Server::new().await;
    for uri in ["/health", "/ready"] {
        assert_eq!(
            server.call(None, Method::GET, uri, None).await,
            (StatusCode::OK, json!({ "status": "ok" }))
        );
    }
}

#[tokio::test]
async fn answers_unknown_paths_and_methods_with_the_error_envelope() {
    let server = Server::new().await;
    for uri in [
        "/v1/nothing",
        "/",
        "/v1/workspaces/ws_031DCp7XMZ9AHIB2uF6QgS/not-a-route",
    ] {
        let (status, body) = server.call(None, Method::GET, uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(
            body,
            json!({
                "data": null,
                "meta": null,
                "error": { "code": "not_found", "message": "There's nothing at this path.", "fields": null },
            }),
        );
    }
    let (status, body) = server.call(None, Method::POST, "/health", None).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(code(&body), "method_not_allowed");
    assert_eq!(body["data"], Value::Null);
}

#[tokio::test]
async fn checks_the_credential_first() {
    let server = Server::new().await;
    for (method, uri) in [
        (Method::GET, "/v1/me"),
        (Method::POST, "/v1/organizations"),
        (Method::GET, "/v1/organizations/not-an-id"),
        (Method::GET, "/v1/organizations/org_031DCp7XMZ9AHIB2uF6QgS/members"),
        (Method::DELETE, "/v1/organizations/x/members/y"),
    ] {
        let (status, body) = server.call(None, method.clone(), uri, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
        assert_eq!(code(&body), "unauthenticated");
    }
}

#[tokio::test]
async fn creates_an_organization_its_caller_owns() {
    let server = Server::new().await;
    let (status, me) = server.get("alice", "/v1/me").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["data"]["organizations"], json!([]));
    assert_eq!(me["data"]["user"]["email"], "alice@people.example");
    assert!(me["data"]["user"]["id"].as_str().unwrap().starts_with("usr_"));

    let (status, created) = server
        .send(
            "alice",
            Method::POST,
            "/v1/organizations",
            json!({ "name": "  Northwind  ", "slug": "northwind" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let org = &created["data"];
    assert!(org["id"].as_str().unwrap().starts_with("org_"));
    assert_eq!(
        (org["name"].as_str(), org["slug"].as_str()),
        (Some("Northwind"), Some("northwind"))
    );
    assert_eq!(org["role"], "owner");
    assert_eq!(
        org["setup"],
        json!({
            "has_app": false, "has_ready_configuration": false, "has_workspace": false,
            "has_api_key": false, "has_installation": false, "has_agent": false, "has_run": false,
        })
    );
    let created_at = org["created_at"].as_str().unwrap();
    assert!(created_at.ends_with('Z') && created_at.len() == 24, "{created_at}");

    let (_, me) = server.get("alice", "/v1/me").await;
    assert_eq!(me["data"]["organizations"], json!([org]));
    let id = org["id"].as_str().unwrap();
    let (status, fetched) = server.get("alice", &format!("/v1/organizations/{id}")).await;
    assert_eq!((status, &fetched["data"]), (StatusCode::OK, org));

    let (status, members) = server.get("alice", &format!("/v1/organizations/{id}/members")).await;
    assert_eq!(status, StatusCode::OK);
    let members = members["data"].as_array().unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(
        (&members[0]["role"], &members[0]["status"], &members[0]["user"]["email"]),
        (&json!("owner"), &json!("active"), &json!("alice@people.example"))
    );
}

#[tokio::test]
async fn validates_a_new_organization() {
    let server = Server::new().await;
    let cases = [
        (json!({}), vec!["name", "slug"]),
        (json!({ "name": "  ", "slug": "ok" }), vec!["name"]),
        (json!({ "name": "x".repeat(61), "slug": "ok" }), vec!["name"]),
        (json!({ "name": "Ok", "slug": "Not_Valid" }), vec!["slug"]),
        (json!({ "name": "Ok", "slug": "-edge" }), vec!["slug"]),
        (json!({ "name": "Ok", "slug": "x".repeat(41) }), vec!["slug"]),
        (json!({ "name": 3, "slug": "ok" }), vec!["name"]),
    ];
    for (body, invalid) in cases {
        let (status, answer) = server
            .send("alice", Method::POST, "/v1/organizations", body.clone())
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(code(&answer), "validation_failed");
        let fields: Vec<&str> = answer["error"]["fields"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(fields, invalid, "{body}");
    }
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/organizations")
        .header("Authorization", "Bearer test:alice")
        .header("Content-Type", "application/json; charset=utf-8")
        .body(Body::from("{not json"))
        .unwrap();
    let response = server.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let (status, answer) = server
        .send(
            "alice",
            Method::POST,
            "/v1/organizations",
            json!({ "name": "Ok", "slug": "settings" }),
        )
        .await;
    assert_eq!(
        (status, code(&answer)),
        (StatusCode::UNPROCESSABLE_ENTITY, "slug_reserved")
    );

    server.organization("alice", "taken").await;
    let (status, answer) = server
        .send(
            "bob",
            Method::POST,
            "/v1/organizations",
            json!({ "name": "Ok", "slug": "taken" }),
        )
        .await;
    assert_eq!((status, code(&answer)), (StatusCode::CONFLICT, "slug_taken"));
    let (_, me) = server.get("bob", "/v1/me").await;
    assert_eq!(me["data"]["organizations"], json!([]), "nothing was created for bob");
}

#[tokio::test]
async fn hides_another_organization_as_404() {
    let server = Server::new().await;
    let northwind = server.organization("alice", "northwind").await;
    let larkspur = server.organization("bob", "larkspur").await;
    let first_member = |body: Value| body["data"][0]["id"].as_str().unwrap().to_owned();
    let alice_member = first_member(
        server
            .get("alice", &format!("/v1/organizations/{northwind}/members"))
            .await
            .1,
    );
    let bob_member = first_member(
        server
            .get("bob", &format!("/v1/organizations/{larkspur}/members"))
            .await
            .1,
    );

    // Bob, an owner elsewhere, tries Northwind's real IDs.
    let attempts = [
        (Method::GET, format!("/v1/organizations/{northwind}"), None),
        (
            Method::PATCH,
            format!("/v1/organizations/{northwind}"),
            Some(json!({ "name": "Taken over" })),
        ),
        (Method::GET, format!("/v1/organizations/{northwind}/members"), None),
        (
            Method::POST,
            format!("/v1/organizations/{northwind}/members"),
            Some(json!({ "email": "bob@people.example", "role": "owner" })),
        ),
        (
            Method::PATCH,
            format!("/v1/organizations/{northwind}/members/{alice_member}"),
            Some(json!({ "role": "viewer" })),
        ),
        (
            Method::DELETE,
            format!("/v1/organizations/{northwind}/members/{alice_member}"),
            None,
        ),
        // His own organization, with Northwind's member.
        (
            Method::PATCH,
            format!("/v1/organizations/{larkspur}/members/{alice_member}"),
            Some(json!({ "role": "viewer" })),
        ),
        (
            Method::DELETE,
            format!("/v1/organizations/{larkspur}/members/{alice_member}"),
            None,
        ),
        // Malformed, bare-UUID and wrong-kind IDs.
        (Method::GET, "/v1/organizations/northwind".to_owned(), None),
        (Method::GET, format!("/v1/organizations/{}", &northwind[4..]), None),
        (Method::GET, format!("/v1/organizations/{bob_member}"), None),
    ];
    for (method, uri, body) in attempts {
        let (status, answer) = server.call(Some("bob"), method.clone(), &uri, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}: {answer}");
        assert_eq!(code(&answer), "not_found");
    }
    // Nothing changed.
    let (_, org) = server.get("alice", &format!("/v1/organizations/{northwind}")).await;
    assert_eq!(org["data"]["name"], "Test");
    let (_, members) = server
        .get("alice", &format!("/v1/organizations/{northwind}/members"))
        .await;
    assert_eq!(members["data"].as_array().unwrap().len(), 1);
    assert_eq!(members["data"][0]["role"], "owner");
}

#[tokio::test]
async fn treats_an_invitation_as_not_yet_a_membership() {
    let server = Server::new().await;
    let northwind = server.organization("alice", "northwind").await;
    let members = format!("/v1/organizations/{northwind}/members");
    server.get("bob", "/v1/me").await;
    let (status, invitation) = server
        .send(
            "alice",
            Method::POST,
            &members,
            json!({ "email": " Bob@People.Example ", "role": "owner" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let invitation = &invitation["data"];
    assert_eq!(
        (
            &invitation["status"],
            &invitation["user"]["email"],
            &invitation["joined_at"]
        ),
        (&json!("invited"), &json!("bob@people.example"), &Value::Null)
    );
    assert_eq!(
        server.get("bob", &format!("/v1/organizations/{northwind}")).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(server.get("bob", "/v1/me").await.1["data"]["organizations"], json!([]));

    let (status, again) = server
        .send(
            "alice",
            Method::POST,
            &members,
            json!({ "email": "bob@people.example", "role": "viewer" }),
        )
        .await;
    assert_eq!((status, code(&again)), (StatusCode::CONFLICT, "already_member"));
    let (status, invalid) = server
        .send(
            "alice",
            Method::POST,
            &members,
            json!({ "email": "not an email", "role": "boss" }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        invalid["error"]["fields"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["email", "role"]
    );
}

#[tokio::test]
async fn lets_each_role_do_only_its_own() {
    let server = Server::new().await;
    let org = server.organization("owner", "northwind").await;
    server.member("owner", &org, "admin", "admin").await;
    let developer = server.member("owner", &org, "developer", "developer").await;
    let viewer = server.member("owner", &org, "viewer", "viewer").await;
    let base = format!("/v1/organizations/{org}");
    let members = format!("{base}/members");

    // Everyone reads.
    for user in ["admin", "developer", "viewer"] {
        assert_eq!(server.get(user, &base).await.0, StatusCode::OK, "{user}");
        assert_eq!(server.get(user, &members).await.0, StatusCode::OK, "{user}");
        assert_eq!(
            server.get(user, "/v1/me").await.1["data"]["organizations"][0]["role"],
            user
        );
    }

    // Developers and viewers change nothing here.
    for user in ["developer", "viewer"] {
        let attempts = [
            (Method::PATCH, base.clone(), Some(json!({ "name": "Renamed" }))),
            (
                Method::POST,
                members.clone(),
                Some(json!({ "email": "new@people.example", "role": "viewer" })),
            ),
            (
                Method::PATCH,
                format!("{members}/{viewer}"),
                Some(json!({ "role": "admin" })),
            ),
            (Method::DELETE, format!("{members}/{viewer}"), None),
        ];
        for (method, uri, body) in attempts {
            let (status, answer) = server.call(Some(user), method.clone(), &uri, body).await;
            assert_eq!(
                (status, code(&answer)),
                (StatusCode::FORBIDDEN, "forbidden"),
                "{user} {method} {uri}"
            );
        }
    }

    // Admins manage settings and members, but not roles or owners.
    assert_eq!(
        server
            .send("admin", Method::PATCH, &base, json!({ "name": "Renamed" }))
            .await
            .0,
        StatusCode::OK
    );
    let invite = |email: &str, role: &str| json!({ "email": email, "role": role });
    assert_eq!(
        server
            .send(
                "admin",
                Method::POST,
                &members,
                invite("new@people.example", "developer")
            )
            .await
            .0,
        StatusCode::CREATED
    );
    let (status, answer) = server
        .send("admin", Method::POST, &members, invite("boss@people.example", "owner"))
        .await;
    assert_eq!(
        (status, code(&answer)),
        (StatusCode::FORBIDDEN, "forbidden"),
        "only owners invite owners"
    );
    assert_eq!(
        server
            .send(
                "admin",
                Method::PATCH,
                &format!("{members}/{developer}"),
                json!({ "role": "viewer" })
            )
            .await
            .0,
        StatusCode::FORBIDDEN,
        "only owners change roles"
    );
    let owner_member = server.get("owner", &members).await.1["data"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, answer) = server
        .call(
            Some("admin"),
            Method::DELETE,
            &format!("{members}/{owner_member}"),
            None,
        )
        .await;
    assert_eq!(
        (status, code(&answer)),
        (StatusCode::FORBIDDEN, "forbidden"),
        "only owners remove an owner"
    );
    let (status, answer) = server
        .call(Some("admin"), Method::DELETE, &format!("{members}/{viewer}"), None)
        .await;
    assert_eq!((status, answer), (StatusCode::NO_CONTENT, Value::Null));

    // Owners change roles, but never leave the organization without one.
    let (status, changed) = server
        .send(
            "owner",
            Method::PATCH,
            &format!("{members}/{developer}"),
            json!({ "role": "viewer" }),
        )
        .await;
    assert_eq!((status, &changed["data"]["role"]), (StatusCode::OK, &json!("viewer")));
    for (method, body) in [
        (Method::PATCH, Some(json!({ "role": "admin" }))),
        (Method::DELETE, None),
    ] {
        let (status, answer) = server
            .call(Some("owner"), method, &format!("{members}/{owner_member}"), body)
            .await;
        assert_eq!(
            (status, code(&answer)),
            (StatusCode::UNPROCESSABLE_ENTITY, "last_owner")
        );
    }
}

/// With `local` sign-in every request is the owner, so a request a browser
/// was tricked into sending must not get through: not addressed to a rebound
/// name, not from another site, and not with a body a form could send.
#[tokio::test]
async fn refuses_what_a_browser_was_tricked_into_sending() {
    let origins = ["http://localhost:5173".to_owned()];
    let server = Server::with_guard(RequestGuard::local(8080, &origins)).await;
    let me = |host: &'static str, origin: Option<&'static str>| {
        let mut headers = vec![("Authorization", "Bearer test:alice"), ("Host", host)];
        if let Some(origin) = origin {
            headers.push(("Origin", origin));
        }
        headers
    };

    for host in ["localhost:8080", "127.0.0.1:8080", "[::1]:8080", "LOCALHOST:8080"] {
        let (status, _) = server.raw(Method::GET, "/v1/me", &me(host, None), "").await;
        assert_eq!(status, StatusCode::OK, "{host}");
    }
    for host in [
        "rebound.attacker.example:8080",
        "localhost:9999",
        "localhost",
        "127.0.0.2:8080",
    ] {
        let (status, body) = server.raw(Method::GET, "/v1/me", &me(host, None), "").await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::FORBIDDEN, "host_not_allowed"),
            "{host}"
        );
    }
    let (status, body) = server
        .raw(Method::GET, "/v1/me", &[("Authorization", "Bearer test:alice")], "")
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "host_not_allowed"),
        "no Host"
    );

    for origin in [
        "http://localhost:5173",
        "http://localhost:5173/",
        "http://localhost:8080",
    ] {
        let (status, _) = server
            .raw(Method::GET, "/v1/me", &me("localhost:8080", Some(origin)), "")
            .await;
        assert_eq!(status, StatusCode::OK, "{origin}");
    }
    for origin in ["https://attacker.example", "null", "http://localhost:5174"] {
        let (status, body) = server
            .raw(Method::GET, "/v1/me", &me("localhost:8080", Some(origin)), "")
            .await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::FORBIDDEN, "origin_not_allowed"),
            "{origin}"
        );
    }

    // A body a cross-site form could send is refused, and creates nothing.
    for content_type in ["text/plain", "application/x-www-form-urlencoded", "multipart/form-data"] {
        let mut headers = me("localhost:8080", None);
        headers.push(("Content-Type", content_type));
        let (status, body) = server
            .raw(
                Method::POST,
                "/v1/organizations",
                &headers,
                r#"{"name":"Evil","slug":"evil"}"#,
            )
            .await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type"),
            "{content_type}"
        );
    }
    let (_, me_now) = server.raw(Method::GET, "/v1/me", &me("localhost:8080", None), "").await;
    assert_eq!(me_now["data"]["organizations"], json!([]));
}

/// CORS answers the Console's origin, and no other: its preflights pass and
/// it may read every answer, errors included; an origin the guard admits
/// for other reasons gets no CORS header, and a foreign one is refused.
#[tokio::test]
async fn answers_cors_for_the_console_origin_only() {
    let console = "http://localhost:5180";
    let guard = RequestGuard::local(8080, &["http://localhost:5173".to_owned()]).with_console("http://LOCALHOST:5180/");
    let server = Server::with_guard(guard).await;
    let header = |headers: &axum::http::HeaderMap, name: &str| {
        headers
            .get_all(name)
            .iter()
            .map(|value| value.to_str().unwrap().to_owned())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let preflight = |origin: &'static str| {
        vec![
            ("Host", "localhost:8080"),
            ("Origin", origin),
            ("Access-Control-Request-Method", "POST"),
            (
                "Access-Control-Request-Headers",
                "authorization, content-type, idempotency-key, zupflow-organization",
            ),
        ]
    };

    // The Console's preflight, for any path: every method and header it sends, no credentials.
    for path in ["/v1/organizations", "/v1/workspaces/ws_x/agents", "/v1/me"] {
        let (status, headers, body) = server.exchange(Method::OPTIONS, path, &preflight(console), "").await;
        assert_eq!((status, &body), (StatusCode::NO_CONTENT, &Value::Null), "{path}");
        assert_eq!(header(&headers, "access-control-allow-origin"), console);
        for method in ["GET", "POST", "PUT", "PATCH", "DELETE"] {
            assert!(
                header(&headers, "access-control-allow-methods").contains(method),
                "{method}"
            );
        }
        for name in [
            "authorization",
            "content-type",
            "idempotency-key",
            "zupflow-organization",
        ] {
            assert!(
                header(&headers, "access-control-allow-headers").contains(name),
                "{name}"
            );
        }
        assert!(header(&headers, "vary").contains("Origin"));
        assert!(!headers.contains_key("access-control-allow-credentials"));
    }

    // Its requests, answered or refused, carry the header, so the Console reads the envelope either way.
    let signed_in = [
        ("Host", "localhost:8080"),
        ("Origin", console),
        ("Authorization", "Bearer test:alice"),
    ];
    let (status, headers, _) = server.exchange(Method::GET, "/v1/me", &signed_in, "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(header(&headers, "access-control-allow-origin"), console);
    assert!(header(&headers, "vary").contains("Origin"));
    let (status, headers, body) = server
        .exchange(
            Method::GET,
            "/v1/me",
            &[("Host", "localhost:8080"), ("Origin", console)],
            "",
        )
        .await;
    assert_eq!((status, code(&body)), (StatusCode::UNAUTHORIZED, "unauthenticated"));
    assert_eq!(header(&headers, "access-control-allow-origin"), console);

    // Another allowed origin is admitted, but gets no CORS: no preflight passes for it, and nothing says it may read.
    let (status, headers, _) = server
        .exchange(Method::OPTIONS, "/v1/me", &preflight("http://localhost:5173"), "")
        .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(!headers.contains_key("access-control-allow-origin"));
    let (status, headers, _) = server
        .exchange(
            Method::GET,
            "/v1/me",
            &[
                ("Host", "localhost:8080"),
                ("Origin", "http://localhost:5173"),
                ("Authorization", "Bearer test:alice"),
            ],
            "",
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("access-control-allow-origin"));

    // A foreign origin is refused before anything, preflight or not, with nothing that lets it read.
    for origin in ["https://attacker.example", "null", "http://localhost:5181"] {
        let (status, headers, body) = server.exchange(Method::OPTIONS, "/v1/me", &preflight(origin), "").await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::FORBIDDEN, "origin_not_allowed"),
            "{origin}"
        );
        assert!(!headers.contains_key("access-control-allow-origin"), "{origin}");
    }

    // A request with no Origin isn't a browser's cross-origin one: no CORS header.
    let (status, headers, _) = server
        .exchange(
            Method::GET,
            "/v1/me",
            &[("Host", "localhost:8080"), ("Authorization", "Bearer test:alice")],
            "",
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("access-control-allow-origin"));

    // Without a Console origin, no origin gets CORS, even an allowed one.
    let server = Server::with_guard(RequestGuard::local(8080, &[console.to_owned()])).await;
    let (status, headers, _) = server
        .exchange(Method::OPTIONS, "/v1/me", &preflight(console), "")
        .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(!headers.contains_key("access-control-allow-origin"));
    assert!(!headers.contains_key("vary"));
}

/// The sign-in settings are the deployment's, for anyone: no credential is
/// needed, and none changes the answer.
#[tokio::test]
async fn tells_anyone_how_the_console_signs_in() {
    let server = Server::new().await;
    for user in [None, Some("alice"), Some("")] {
        let (status, body) = server.call(user, Method::GET, "/v1/sign-in-settings", None).await;
        assert_eq!(
            (status, body),
            (
                StatusCode::OK,
                json!({ "data": { "mode": "local" }, "meta": null, "error": null })
            ),
            "{user:?}"
        );
    }
    let (status, body) = server
        .raw(
            Method::GET,
            "/v1/sign-in-settings",
            &[("Authorization", "Bearer not-a-token")],
            "",
        )
        .await;
    assert_eq!((status, &body["data"]["mode"]), (StatusCode::OK, &json!("local")));
}

#[tokio::test]
async fn refuses_a_reserved_slug_on_rename_too() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let (status, body) = server
        .send(
            "alice",
            Method::PATCH,
            &format!("/v1/organizations/{org}"),
            json!({ "slug": "api" }),
        )
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "slug_reserved")
    );
    let (status, body) = server
        .send(
            "alice",
            Method::PATCH,
            &format!("/v1/organizations/{org}"),
            json!({ "slug": "northwind-2" }),
        )
        .await;
    assert_eq!((status, &body["data"]["slug"]), (StatusCode::OK, &json!("northwind-2")));
}

/// Invite emails follow the contract's rule, so every `User.email` the API
/// answers with is one the contract accepts.
#[tokio::test]
async fn invites_only_addresses_the_contract_accepts() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let members = format!("/v1/organizations/{org}/members");
    for email in [
        "a@b..c",
        "a@b.c",
        ".a@b.co",
        "a..b@c.co",
        "a.@b.co",
        "a@-b.co",
        "x!#$%@b.co",
        "é@b.co",
        "a@b.co(comment)",
        "a'@b.co",
        "a@b.c0",
        "a@b",
        "@b.co",
        "a@@b.co",
        "a b@c.co",
    ] {
        let (status, body) = server
            .send(
                "alice",
                Method::POST,
                &members,
                json!({ "email": email, "role": "viewer" }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{email}");
        assert!(body["error"]["fields"]["email"].is_string(), "{email}");
    }
    for email in [
        "o'brien+tag@mail.people.example",
        "a_b-c@x-1.people.example",
        "UPPER@People.Example",
    ] {
        let (status, body) = server
            .send(
                "alice",
                Method::POST,
                &members,
                json!({ "email": email, "role": "viewer" }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{email}: {body}");
        assert_eq!(body["data"]["user"]["email"], email.to_lowercase());
    }
}
