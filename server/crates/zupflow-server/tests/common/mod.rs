//! A test server over a real database, signing in whoever a
//! `Bearer test:{name}` token names, or presenting an API key.

#![allow(dead_code, clippy::unwrap_used)]

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

pub struct TestIdentity;

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

pub struct Server {
    pub app: axum::Router,
    pub directory: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// A request: who, method, path, the organization it names, and a body.
pub struct Call<'a> {
    pub user: &'a str,
    pub method: Method,
    pub uri: String,
    pub organization: Option<&'a str>,
    pub body: Option<Value>,
}

pub fn call<'a>(user: &'a str, method: Method, uri: impl Into<String>) -> Call<'a> {
    Call {
        user,
        method,
        uri: uri.into(),
        organization: None,
        body: None,
    }
}

impl<'a> Call<'a> {
    pub fn naming(mut self, organization: &'a str) -> Self {
        self.organization = Some(organization);
        self
    }

    pub fn body(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }
}

impl Server {
    pub async fn new() -> Self {
        Self::with_identity(Arc::new(TestIdentity)).await
    }

    /// A server signing Console users in through `identity`.
    pub async fn with_identity(identity: Arc<dyn Identity>) -> Self {
        let directory = std::env::temp_dir().join(format!("zupflow-ws-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir(&directory).unwrap();
        let database = Database::open(&directory.join("zupflow.sqlite")).await.unwrap();
        let state = AppState {
            identity,
            organizations: Arc::new(SqliteOrganizationStore::new(database.clone())),
            workspaces: Arc::new(SqliteWorkspaceStore::new(database.clone())),
            api_keys: Arc::new(SqliteApiKeyStore::new(database)),
            api_key_hasher: Arc::new(zupflow_core::api_keys::ApiKeyHasher::new([7; 32])),
        };
        Self {
            app: zupflow_server::app(state, RequestGuard::default()),
            directory,
        }
    }

    pub async fn send(&self, call: Call<'_>) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(call.method)
            .uri(call.uri)
            .header("Authorization", bearer(call.user));
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

    pub async fn organization(&self, user: &str, slug: &str) -> String {
        let (status, body) = self
            .send(call(user, Method::POST, "/v1/organizations").body(json!({ "name": "Test", "slug": slug })))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["data"]["id"].as_str().unwrap().to_owned()
    }

    pub async fn workspace(&self, user: &str, org: &str, slug: &str, environment: &str) -> Value {
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
    pub async fn member(&self, owner: &str, org: &str, user: &str, role: &str) {
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

pub fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

/// `test:{name}` signs in a Console user; an API key's secret is sent as is.
pub fn bearer(who: &str) -> String {
    if who.starts_with("zf_") {
        format!("Bearer {who}")
    } else {
        format!("Bearer test:{who}")
    }
}
