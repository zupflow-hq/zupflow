//! Workspace routes end to end: the organization named by the request
//! (decision 0023), isolation across organizations, roles, and paged lists.

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

/// A request: who, method, path, the organization it names, and a body.
struct Call<'a> {
    user: &'a str,
    method: Method,
    uri: String,
    organization: Option<&'a str>,
    body: Option<Value>,
}

fn call<'a>(user: &'a str, method: Method, uri: impl Into<String>) -> Call<'a> {
    Call {
        user,
        method,
        uri: uri.into(),
        organization: None,
        body: None,
    }
}

impl<'a> Call<'a> {
    fn naming(mut self, organization: &'a str) -> Self {
        self.organization = Some(organization);
        self
    }

    fn body(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }
}

impl Server {
    async fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("zupflow-ws-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir(&directory).unwrap();
        let database = Database::open(&directory.join("zupflow.sqlite")).await.unwrap();
        let state = AppState {
            identity: Arc::new(TestIdentity),
            organizations: Arc::new(SqliteOrganizationStore::new(database.clone())),
            workspaces: Arc::new(SqliteWorkspaceStore::new(database.clone())),
            apps: Arc::new(zupflow_sqlite::SqliteAppStore::new(database.clone())),
            keys: Arc::new(zupflow_basic::local_keys::LocalKeyService::from_server_secret(&[7; 32])),
            public_url: "http://localhost:8080".to_owned(),
            api_keys: Arc::new(SqliteApiKeyStore::new(database.clone())),
            idempotency: Arc::new(zupflow_sqlite::SqliteIdempotencyStore::new(database)),
            fingerprinter: Arc::new(zupflow_core::idempotency::Fingerprinter::new([7; 32])),
            api_key_hasher: Arc::new(zupflow_core::api_keys::ApiKeyHasher::new([7; 32])),
        };
        Self {
            app: zupflow_server::app(state, RequestGuard::default()),
            directory,
        }
    }

    async fn send(&self, call: Call<'_>) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(call.method)
            .uri(call.uri)
            .header("Authorization", format!("Bearer test:{}", call.user));
        if let Some(organization) = call.organization {
            request = request.header("Zupflow-Organization", organization);
        }
        let request = match call.body {
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

    async fn organization(&self, user: &str, slug: &str) -> String {
        let (status, body) = self
            .send(call(user, Method::POST, "/v1/organizations").body(json!({ "name": "Test", "slug": slug })))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["data"]["id"].as_str().unwrap().to_owned()
    }

    async fn workspace(&self, user: &str, org: &str, slug: &str, environment: &str) -> Value {
        let (status, body) = self
            .send(
                call(user, Method::POST, format!("/v1/organizations/{org}/workspaces")).body(json!({
                    "name": slug.to_uppercase(), "slug": slug, "environment": environment, "external_id": null,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["data"].clone()
    }

    /// Makes `user` an active member of `org` with `role`, straight in the
    /// database: the API has no invitation acceptance yet.
    async fn member(&self, owner: &str, org: &str, user: &str, role: &str) {
        self.send(call(user, Method::GET, "/v1/me")).await;
        let (status, _) = self
            .send(
                call(owner, Method::POST, format!("/v1/organizations/{org}/members"))
                    .body(json!({ "email": format!("{user}@people.example"), "role": role })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED);
        let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", self.directory.join("zupflow.sqlite").display()))
            .await
            .unwrap();
        sqlx::query(
            "UPDATE members SET status = 'active', joined_at = 0 \
             WHERE user_id = (SELECT id FROM users WHERE email = ?)",
        )
        .bind(format!("{user}@people.example"))
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
    }
}

fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn creates_and_reads_a_workspace_in_its_organization() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let (status, created) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/workspaces")).body(json!({
                "name": "  Acme  ", "slug": "acme", "environment": "production", "external_id": "  tenant-42 ",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let workspace = &created["data"];
    assert!(workspace["id"].as_str().unwrap().starts_with("ws_"));
    assert_eq!(
        (
            &workspace["name"],
            &workspace["organization_id"],
            &workspace["external_id"]
        ),
        (&json!("Acme"), &json!(org), &json!("tenant-42"))
    );
    assert_eq!(
        workspace["stats"],
        json!({ "agents": 0, "installations": 0, "runs_last_24h": 0 })
    );
    let id = workspace["id"].as_str().unwrap();
    let (status, fetched) = server
        .send(call("alice", Method::GET, format!("/v1/workspaces/{id}")).naming(&org))
        .await;
    assert_eq!((status, &fetched["data"]), (StatusCode::OK, workspace));
    let (_, organization) = server
        .send(call("alice", Method::GET, format!("/v1/organizations/{org}")))
        .await;
    assert_eq!(organization["data"]["setup"]["has_workspace"], true);

    let (status, body) = server
        .send(call("alice", Method::GET, format!("/v1/workspaces/{id}")))
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::BAD_REQUEST, "organization_required"),
        "a Console request names its organization"
    );
    let (status, body) = server
        .send(call("alice", Method::GET, format!("/v1/workspaces/{id}")).naming(" "))
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::BAD_REQUEST, "organization_required"),
        "an empty header names none"
    );
}

#[tokio::test]
async fn validates_a_new_workspace() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let path = format!("/v1/organizations/{org}/workspaces");
    let cases = [
        (json!({}), vec!["environment", "external_id", "name", "slug"]),
        (
            json!({ "name": "A", "slug": "a", "environment": "staging", "external_id": null }),
            vec!["environment"],
        ),
        (
            json!({ "name": "A", "slug": "a", "environment": "sandbox", "external_id": "x".repeat(121) }),
            vec!["external_id"],
        ),
        (
            json!({ "name": "A", "slug": "a", "environment": "sandbox", "external_id": 7 }),
            vec!["external_id"],
        ),
    ];
    for (body, invalid) in cases {
        let (status, answer) = server.send(call("alice", Method::POST, &path).body(body.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let fields: Vec<&str> = answer["error"]["fields"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(fields, invalid, "{body}");
    }
    let (status, created) = server
        .send(call("alice", Method::POST, &path).body(json!({
            "name": "A", "slug": "a", "environment": "sandbox", "external_id": "   ",
        })))
        .await;
    assert_eq!(
        (status, &created["data"]["external_id"]),
        (StatusCode::CREATED, &Value::Null)
    );
    let (status, taken) = server
        .send(call("alice", Method::POST, &path).body(json!({
            "name": "B", "slug": "a", "environment": "production", "external_id": null,
        })))
        .await;
    assert_eq!((status, code(&taken)), (StatusCode::CONFLICT, "slug_taken"));
}

#[tokio::test]
async fn hides_another_organizations_workspace_as_404() {
    let server = Server::new().await;
    let northwind = server.organization("alice", "northwind").await;
    let larkspur = server.organization("bob", "larkspur").await;
    let acme = server.workspace("alice", &northwind, "acme", "production").await;
    let harbor = server.workspace("bob", &larkspur, "harbor", "production").await;
    let acme_id = acme["id"].as_str().unwrap();
    let harbor_id = harbor["id"].as_str().unwrap();

    let attempts = [
        // Bob names his own organization with Northwind's workspace.
        (Some(larkspur.as_str()), Method::GET, acme_id.to_owned(), None),
        (
            Some(larkspur.as_str()),
            Method::PATCH,
            acme_id.to_owned(),
            Some(json!({ "name": "Taken" })),
        ),
        (Some(larkspur.as_str()), Method::DELETE, acme_id.to_owned(), None),
        // Bob names Northwind, which he doesn't belong to.
        (Some(northwind.as_str()), Method::GET, acme_id.to_owned(), None),
        (
            Some(northwind.as_str()),
            Method::PATCH,
            acme_id.to_owned(),
            Some(json!({ "name": "Taken" })),
        ),
        (Some(northwind.as_str()), Method::DELETE, acme_id.to_owned(), None),
        // Malformed, bare-UUID and wrong-kind IDs, and a malformed organization.
        (Some(larkspur.as_str()), Method::GET, "acme".to_owned(), None),
        (Some(larkspur.as_str()), Method::GET, acme_id[3..].to_owned(), None),
        (Some(larkspur.as_str()), Method::GET, larkspur.clone(), None),
        (Some("org_nope"), Method::GET, harbor_id.to_owned(), None),
    ];
    for (organization, method, workspace, body) in attempts {
        let mut request = call("bob", method.clone(), format!("/v1/workspaces/{workspace}"));
        if let Some(organization) = organization {
            request = request.naming(organization);
        }
        if let Some(body) = body {
            request = request.body(body);
        }
        let (status, answer) = server.send(request).await;
        assert_eq!(
            (status, code(&answer)),
            (StatusCode::NOT_FOUND, "not_found"),
            "{method} {workspace} naming {organization:?}"
        );
    }
    for query in ["", "?q=a&q=b", "?limit=x&limit=y"] {
        let (status, list) = server
            .send(call(
                "bob",
                Method::GET,
                format!("/v1/organizations/{northwind}/workspaces{query}"),
            ))
            .await;
        assert_eq!((status, code(&list)), (StatusCode::NOT_FOUND, "not_found"), "{query}");
    }
    // Nothing changed.
    let (status, still) = server
        .send(call("alice", Method::GET, format!("/v1/workspaces/{acme_id}")).naming(&northwind))
        .await;
    assert_eq!((status, &still["data"]["name"]), (StatusCode::OK, &json!("ACME")));
    let (_, theirs) = server
        .send(call(
            "bob",
            Method::GET,
            format!("/v1/organizations/{larkspur}/workspaces"),
        ))
        .await;
    assert_eq!(theirs["data"].as_array().unwrap().len(), 1, "bob sees only his own");
}

#[tokio::test]
async fn lets_each_role_do_only_its_own() {
    let server = Server::new().await;
    let org = server.organization("owner", "northwind").await;
    for (user, role) in [("admin", "admin"), ("developer", "developer"), ("viewer", "viewer")] {
        server.member("owner", &org, user, role).await;
    }
    let acme = server.workspace("owner", &org, "acme", "production").await;
    let path = format!("/v1/workspaces/{}", acme["id"].as_str().unwrap());
    let create = |user: &'static str, slug: &str| {
        call(user, Method::POST, format!("/v1/organizations/{org}/workspaces")).body(json!({
            "name": "New", "slug": slug, "environment": "sandbox", "external_id": null,
        }))
    };

    for user in ["admin", "developer", "viewer"] {
        assert_eq!(
            server.send(call(user, Method::GET, &path).naming(&org)).await.0,
            StatusCode::OK
        );
    }
    for user in ["developer", "viewer"] {
        let attempts = [
            create(user, "nope"),
            call(user, Method::PATCH, &path)
                .naming(&org)
                .body(json!({ "name": "Renamed" })),
            call(user, Method::DELETE, &path).naming(&org),
        ];
        for attempt in attempts {
            let (status, body) = server.send(attempt).await;
            assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, "forbidden"), "{user}");
        }
    }

    // The role is checked before the body is read.
    let request = Request::builder()
        .method(Method::PATCH)
        .uri(&path)
        .header("Authorization", "Bearer test:viewer")
        .header("Zupflow-Organization", &org)
        .header("Content-Type", "text/plain")
        .body(Body::from("{not json"))
        .unwrap();
    let response = server.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    assert_eq!(server.send(create("admin", "by-admin")).await.0, StatusCode::CREATED);
    let (status, renamed) = server
        .send(
            call("admin", Method::PATCH, &path)
                .naming(&org)
                .body(json!({ "name": "Acme Inc", "external_id": "t-1" })),
        )
        .await;
    assert_eq!(
        (status, &renamed["data"]["name"], &renamed["data"]["external_id"]),
        (StatusCode::OK, &json!("Acme Inc"), &json!("t-1"))
    );
    let (status, body) = server
        .send(
            call("admin", Method::PATCH, &path)
                .naming(&org)
                .body(json!({ "environment": "sandbox" })),
        )
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "environment_immutable")
    );
    assert_eq!(
        server.send(call("admin", Method::DELETE, &path).naming(&org)).await.0,
        StatusCode::FORBIDDEN,
        "only owners delete"
    );
    assert_eq!(
        server.send(call("owner", Method::DELETE, &path).naming(&org)).await,
        (StatusCode::NO_CONTENT, Value::Null)
    );
    assert_eq!(
        server.send(call("owner", Method::GET, &path).naming(&org)).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn lists_with_filters_and_pages() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    for (slug, environment) in [
        ("delta", "production"),
        ("alpha", "sandbox"),
        ("charlie", "production"),
        ("bravo", "sandbox"),
        ("echo", "production"),
    ] {
        server.workspace("alice", &org, slug, environment).await;
    }
    let base = format!("/v1/organizations/{org}/workspaces");
    let list = |query: &str| server.send(call("alice", Method::GET, format!("{base}{query}")));

    let (status, first) = list("?limit=2").await;
    assert_eq!(status, StatusCode::OK);
    let names = |body: &Value| -> Vec<String> {
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|workspace| workspace["slug"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(names(&first), ["alpha", "bravo"]);
    assert_eq!(first["meta"]["pagination"]["total"], 5);
    let cursor = first["meta"]["pagination"]["next_cursor"].as_str().unwrap().to_owned();
    let (_, second) = list(&format!("?limit=2&cursor={cursor}")).await;
    assert_eq!(names(&second), ["charlie", "delta"]);
    let cursor = second["meta"]["pagination"]["next_cursor"].as_str().unwrap().to_owned();
    let (_, last) = list(&format!("?limit=2&cursor={cursor}")).await;
    assert_eq!(
        (names(&last), &last["meta"]["pagination"]["next_cursor"]),
        (vec!["echo".to_owned()], &Value::Null)
    );

    assert_eq!(names(&list("?environment=sandbox").await.1), ["alpha", "bravo"]);
    assert_eq!(names(&list("?q=HAR&sort=activity").await.1), ["charlie"]);
    assert_eq!(
        list("?limit=0").await.1["data"].as_array().unwrap().len(),
        1,
        "clamped to 1"
    );
    assert_eq!(
        list("?limit=").await.1["data"].as_array().unwrap().len(),
        1,
        "empty is 0, clamped"
    );
    assert_eq!(
        names(&list("?q=charlie&q=alpha").await.1),
        ["charlie"],
        "a repeated parameter takes its first value"
    );

    // The cursor leaves the server, so it carries the prefixed ID, never a bare UUID.
    let raw = {
        use base64::Engine;
        let cursor = first["meta"]["pagination"]["next_cursor"].as_str().unwrap();
        String::from_utf8(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(cursor).unwrap()).unwrap()
    };
    let bravo = first["data"][1]["id"].as_str().unwrap();
    assert!(raw.starts_with(&format!("{bravo}\n")), "{raw}");
    assert_eq!(
        list("?limit=two").await.1["data"].as_array().unwrap().len(),
        5,
        "not an integer: the default"
    );
    let (status, body) = list("?environment=staging").await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, "validation_failed"));
    let (status, body) = list("?cursor=forged").await;
    assert_eq!(
        (status, &body["error"]["fields"]["cursor"].is_string()),
        (StatusCode::BAD_REQUEST, &true)
    );
}
