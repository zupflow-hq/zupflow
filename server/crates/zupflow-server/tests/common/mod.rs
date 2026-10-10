//! A test server over a real database, signing in whoever a
//! `Bearer test:{name}` token names, or presenting an API key.

#![allow(dead_code, clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use zupflow_core::identity::{Identity, IdentityError, Profile, VerifiedIdentity};
use zupflow_core::outbound::{Limits, OutboundError, OutboundHttp, OutboundRequest, OutboundResponse};
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

/// What a provider was sent: the URL, and the credential's header and value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    pub url: String,
    pub credential: Option<(String, String)>,
}

/// Stands in for every provider: records what it's sent and answers what
/// the test chose, `200` until told otherwise, or what it scripted for a URL.
pub struct Providers {
    pub sent: Mutex<Vec<Sent>>,
    /// Each request's URL and body, in order.
    pub bodies: Mutex<Vec<(String, Option<String>)>>,
    pub answer: Mutex<Result<u16, OutboundError>>,
    /// A URL's own answer: its status and body.
    pub scripted: Mutex<Vec<(String, u16, Value)>>,
    /// MCP servers by URL, and the tools each lists.
    pub mcp: Mutex<Vec<(String, Value)>>,
}

impl Providers {
    pub fn answer(&self, answer: Result<u16, OutboundError>) {
        *self.answer.lock().unwrap() = answer;
    }

    /// From now on, `url` answers `status` with `body`.
    pub fn script(&self, url: &str, status: u16, body: Value) {
        let mut scripted = self.scripted.lock().unwrap();
        scripted.retain(|(scripted, _, _)| scripted != url);
        scripted.push((url.to_owned(), status, body));
    }

    /// From now on, `url` is an MCP server listing `tools`.
    pub fn serve_mcp(&self, url: &str, tools: Value) {
        let mut mcp = self.mcp.lock().unwrap();
        mcp.retain(|(served, _)| served != url);
        mcp.push((url.to_owned(), tools));
    }

    pub fn bodies(&self) -> Vec<(String, Option<String>)> {
        self.bodies.lock().unwrap().clone()
    }

    pub fn sent(&self) -> Vec<Sent> {
        self.sent.lock().unwrap().clone()
    }
}

#[async_trait]
impl OutboundHttp for Providers {
    async fn send(&self, request: OutboundRequest, _: Limits) -> Result<OutboundResponse, OutboundError> {
        self.bodies.lock().unwrap().push((
            request.url.clone(),
            request
                .body
                .as_ref()
                .map(|body| String::from_utf8_lossy(body).into_owned()),
        ));
        let scripted = self
            .scripted
            .lock()
            .unwrap()
            .iter()
            .find(|(url, _, _)| *url == request.url)
            .cloned();
        let mcp = self
            .mcp
            .lock()
            .unwrap()
            .iter()
            .find(|(url, _)| *url == request.url)
            .map(|(_, tools)| tools.clone());
        self.sent.lock().unwrap().push(Sent {
            url: request.url,
            credential: request
                .credential
                .map(|credential| (credential.header, credential.value.to_string())),
        });
        if let (Some(tools), Some(body)) = (mcp, &request.body) {
            let message: Value = serde_json::from_slice(body).unwrap();
            let id = message["id"].clone();
            let (status, answer) = match message["method"].as_str() {
                Some("initialize") => (
                    200,
                    json!({ "jsonrpc": "2.0", "id": id, "result": { "protocolVersion": "2025-06-18", "capabilities": { "tools": {} } } }),
                ),
                Some("tools/list") => (200, json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools } })),
                _ => (202, Value::Null),
            };
            return Ok(OutboundResponse {
                status,
                headers: vec![
                    ("content-type".to_owned(), "application/json".to_owned()),
                    ("mcp-session-id".to_owned(), "session-1".to_owned()),
                ],
                body: if answer.is_null() {
                    Vec::new()
                } else {
                    answer.to_string().into_bytes()
                },
            });
        }
        if let Some((_, status, body)) = scripted {
            return Ok(OutboundResponse {
                status,
                headers: Vec::new(),
                body: body.to_string().into_bytes(),
            });
        }
        let status = (*self.answer.lock().unwrap())?;
        Ok(OutboundResponse {
            status,
            headers: Vec::new(),
            body: b"{\"error\":\"words from the provider\"}".to_vec(),
        })
    }
}

pub struct Server {
    pub app: axum::Router,
    /// The state the router serves, for driving background work.
    pub state: AppState,
    pub directory: PathBuf,
    pub providers: Arc<Providers>,
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
    pub idempotency_key: Option<&'a str>,
}

pub fn call<'a>(user: &'a str, method: Method, uri: impl Into<String>) -> Call<'a> {
    Call {
        user,
        method,
        uri: uri.into(),
        organization: None,
        body: None,
        idempotency_key: None,
    }
}

impl<'a> Call<'a> {
    pub fn naming(mut self, organization: &'a str) -> Self {
        self.organization = Some(organization);
        self
    }

    pub fn idempotent(mut self, key: &'a str) -> Self {
        self.idempotency_key = Some(key);
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
        let providers = Arc::new(Providers {
            sent: Mutex::new(Vec::new()),
            bodies: Mutex::new(Vec::new()),
            answer: Mutex::new(Ok(200)),
            scripted: Mutex::new(Vec::new()),
            mcp: Mutex::new(Vec::new()),
        });
        let state = AppState {
            identity,
            organizations: Arc::new(SqliteOrganizationStore::new(database.clone())),
            workspaces: Arc::new(SqliteWorkspaceStore::new(database.clone())),
            apps: Arc::new(zupflow_sqlite::SqliteAppStore::new(database.clone())),
            installations: Arc::new(zupflow_sqlite::SqliteInstallationStore::new(database.clone())),
            connect: Arc::new(zupflow_sqlite::SqliteConnectStore::new(database.clone())),
            jobs: Arc::new(zupflow_sqlite::SqliteJobQueue::new(database.clone())),
            tools: Arc::new(zupflow_sqlite::SqliteToolRegistry::new(database.clone())),
            keys: Arc::new(zupflow_basic::local_keys::LocalKeyService::from_server_secret(&[7; 32])),
            outbound: providers.clone(),
            public_url: "http://localhost:8080".to_owned(),
            api_keys: Arc::new(SqliteApiKeyStore::new(database.clone())),
            idempotency: Arc::new(zupflow_sqlite::SqliteIdempotencyStore::new(database)),
            fingerprinter: Arc::new(zupflow_core::idempotency::Fingerprinter::new([7; 32])),
            api_key_hasher: Arc::new(zupflow_core::api_keys::ApiKeyHasher::new([7; 32])),
        };
        Self {
            app: zupflow_server::app(state.clone(), RequestGuard::default()),
            state,
            directory,
            providers,
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
        if let Some(key) = call.idempotency_key {
            request = request.header("Idempotency-Key", key);
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

    /// A browser arriving at `uri` with no credential, as a provider's
    /// redirect sends it: the status, the `Location` it's sent on to, and
    /// the body.
    pub async fn visit(&self, uri: &str) -> (StatusCode, Option<String>, Value) {
        let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get("Location")
            .map(|value| value.to_str().unwrap().to_owned());
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, location, json)
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

/// The template a test installation made through the store connected under.
pub fn token_snapshot() -> zupflow_core::installations::TemplateSnapshot {
    use zupflow_core::apps::{AuthTemplate, TokenLocation, TokenPlacement};
    zupflow_core::installations::TemplateSnapshot {
        auth: AuthTemplate::Token {
            inputs: Vec::new(),
            placement: TokenPlacement {
                location: TokenLocation::Header,
                name: "Authorization".to_owned(),
                prefix: "Bearer ".to_owned(),
            },
            verify_url: None,
            instructions: String::new(),
            docs_url: None,
        },
        app_version: 1,
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
