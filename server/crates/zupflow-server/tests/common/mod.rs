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

/// A key the scripted providers accept, and the model a test environment's
/// default routes to through it.
pub const PROVIDER_KEY: &str = "sk-test-0123456789abcdefWXYZ";
pub const DEFAULT_MODEL: &str = "gpt-5-mini";

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

/// One request as it was sent: its URL, headers and body.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// One request an MCP server physically received, as it arrived: its URL,
/// its JSON-RPC method, the tool and arguments of a `tools/call`, and the
/// credential it carried. Kept by the fake server itself, apart from
/// anything the runtime records, so a request the runtime lost track of
/// still shows here.
#[derive(Debug, Clone, PartialEq)]
pub struct Received {
    pub url: String,
    pub method: String,
    pub tool: Option<String>,
    pub arguments: Value,
    /// The credential's header and value.
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
    /// URLs that answer a request without a credential `401`, with this
    /// `WWW-Authenticate`.
    pub challenges: Mutex<Vec<(String, String)>>,
    /// Each `tools/call` an MCP server was sent: the tool and its arguments.
    pub tool_calls: Mutex<Vec<(String, Value)>>,
    /// The HTTP status MCP servers answer `tools/call` with; `200` answers
    /// the call's result.
    pub tool_status: Mutex<u16>,
    /// Every request, with its headers, in order.
    pub requests: Mutex<Vec<Recorded>>,
    /// The MCP servers' ledger: every request one of them received, in order.
    pub ledger: Mutex<Vec<Received>>,
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

    /// From now on, `url` answers a request without a credential `401`,
    /// with `www_authenticate`.
    pub fn challenge(&self, url: &str, www_authenticate: &str) {
        let mut challenges = self.challenges.lock().unwrap();
        challenges.retain(|(challenged, _)| challenged != url);
        challenges.push((url.to_owned(), www_authenticate.to_owned()));
    }

    /// The URLs requested so far, in order.
    pub fn urls(&self) -> Vec<String> {
        self.sent().into_iter().map(|sent| sent.url).collect()
    }

    /// From now on, `url` is an MCP server listing `tools`.
    pub fn serve_mcp(&self, url: &str, tools: Value) {
        let mut mcp = self.mcp.lock().unwrap();
        mcp.retain(|(served, _)| served != url);
        mcp.push((url.to_owned(), tools));
    }

    /// The tools called so far, with their arguments, in order.
    pub fn tool_calls(&self) -> Vec<(String, Value)> {
        self.tool_calls.lock().unwrap().clone()
    }

    pub fn bodies(&self) -> Vec<(String, Option<String>)> {
        self.bodies.lock().unwrap().clone()
    }

    /// The requests sent to `url`, in order.
    pub fn requests_to(&self, url: &str) -> Vec<Recorded> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.url == url)
            .cloned()
            .collect()
    }

    pub fn sent(&self) -> Vec<Sent> {
        self.sent.lock().unwrap().clone()
    }

    /// Every request the MCP servers received, in order.
    pub fn ledger(&self) -> Vec<Received> {
        self.ledger.lock().unwrap().clone()
    }

    /// The `tools/call` requests the MCP servers received, in order.
    pub fn received_calls(&self) -> Vec<Received> {
        self.ledger()
            .into_iter()
            .filter(|received| received.method == "tools/call")
            .collect()
    }
}

#[async_trait]
impl OutboundHttp for Providers {
    async fn send(&self, request: OutboundRequest, _: Limits) -> Result<OutboundResponse, OutboundError> {
        self.requests.lock().unwrap().push(Recorded {
            url: request.url.clone(),
            headers: request.headers.clone(),
            body: request.body.clone().unwrap_or_default(),
        });
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
        if mcp.is_some() {
            let message: Value = request
                .body
                .as_deref()
                .and_then(|body| serde_json::from_slice(body).ok())
                .unwrap_or_default();
            self.ledger.lock().unwrap().push(Received {
                url: request.url.clone(),
                method: message["method"].as_str().unwrap_or_default().to_owned(),
                tool: message["params"]["name"].as_str().map(str::to_owned),
                arguments: message["params"]["arguments"].clone(),
                credential: request
                    .credential
                    .as_ref()
                    .map(|credential| (credential.header.clone(), credential.value.to_string())),
            });
        }
        let challenge = self
            .challenges
            .lock()
            .unwrap()
            .iter()
            .find(|(url, _)| *url == request.url)
            .map(|(_, challenge)| challenge.clone())
            .filter(|_| request.credential.is_none());
        self.sent.lock().unwrap().push(Sent {
            url: request.url,
            credential: request
                .credential
                .map(|credential| (credential.header, credential.value.to_string())),
        });
        if let Some(challenge) = challenge {
            return Ok(OutboundResponse {
                status: 401,
                headers: vec![("WWW-Authenticate".to_owned(), challenge)],
                body: Vec::new(),
            });
        }
        if let (Some(tools), Some(body)) = (mcp, &request.body) {
            let message: Value = serde_json::from_slice(body).unwrap();
            let id = message["id"].clone();
            let (status, answer) = match message["method"].as_str() {
                Some("initialize") => (
                    200,
                    json!({ "jsonrpc": "2.0", "id": id, "result": { "protocolVersion": "2025-06-18", "capabilities": { "tools": {} } } }),
                ),
                Some("tools/list") => (200, json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools } })),
                Some("tools/call") => {
                    let name = message["params"]["name"].as_str().unwrap_or_default().to_owned();
                    let arguments = message["params"]["arguments"].clone();
                    self.tool_calls.lock().unwrap().push((name.clone(), arguments.clone()));
                    let status = *self.tool_status.lock().unwrap();
                    if status != 200 {
                        return Ok(OutboundResponse {
                            status,
                            headers: Vec::new(),
                            body: Vec::new(),
                        });
                    }
                    // Arguments with `"fail": true` get the tool's own error,
                    // echoing them.
                    if arguments["fail"] == true {
                        (
                            200,
                            json!({ "jsonrpc": "2.0", "id": id, "result": {
                                "isError": true,
                                "content": [{ "type": "text", "text": format!("{name} failed on {arguments}") }],
                            } }),
                        )
                    } else {
                        (
                            200,
                            json!({ "jsonrpc": "2.0", "id": id, "result": {
                                "content": [{ "type": "text", "text": format!("{name} done") }],
                                "structuredContent": { "tool": name, "arguments": arguments },
                            } }),
                        )
                    }
                }
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
    /// The model providers' APIs, faked: what the provider adapters reach
    /// in a server made [`Server::with_model_providers`].
    pub models: Arc<zupflow_providers::fake::FakeProviders>,
    /// The database the stores share, for a test that builds another store over it.
    pub database: Database,
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

    /// A server whose runs and model connections reach the model providers
    /// through the real adapters, over [`Server::models`]: the providers'
    /// APIs faked, so no real one is ever reached.
    pub async fn with_model_providers() -> Self {
        Self::build(Arc::new(TestIdentity), true).await
    }

    /// A server signing Console users in through `identity`.
    pub async fn with_identity(identity: Arc<dyn Identity>) -> Self {
        Self::build(identity, false).await
    }

    async fn build(identity: Arc<dyn Identity>, adapters: bool) -> Self {
        let directory = std::env::temp_dir().join(format!("zupflow-ws-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir(&directory).unwrap();
        let database = Database::open(&directory.join("zupflow.sqlite")).await.unwrap();
        let providers = Arc::new(Providers {
            sent: Mutex::new(Vec::new()),
            bodies: Mutex::new(Vec::new()),
            answer: Mutex::new(Ok(200)),
            scripted: Mutex::new(Vec::new()),
            mcp: Mutex::new(Vec::new()),
            challenges: Mutex::new(Vec::new()),
            tool_calls: Mutex::new(Vec::new()),
            tool_status: Mutex::new(200),
            requests: Mutex::new(Vec::new()),
            ledger: Mutex::new(Vec::new()),
        });
        let models = Arc::new(zupflow_providers::fake::FakeProviders::new());
        let (provider, model_catalog): (
            Arc<dyn zupflow_core::models::ModelProvider>,
            Arc<dyn zupflow_core::model_providers::ModelProviderCatalog>,
        ) = if adapters {
            (
                Arc::new(zupflow_providers::HttpModelProvider::new(models.clone())),
                Arc::new(zupflow_providers::HttpProviderCatalog::new(models.clone())),
            )
        } else {
            (
                Arc::new(zupflow_basic::scripted_model::ScriptedModel::serving(
                    zupflow_basic::scripted_providers::ScriptedProviders::tool_models(),
                )),
                Arc::new(zupflow_basic::scripted_providers::ScriptedProviders),
            )
        };
        let state = AppState {
            clock: Arc::new(zupflow_core::time::SystemClock),
            identity,
            organizations: Arc::new(SqliteOrganizationStore::new(database.clone())),
            workspaces: Arc::new(SqliteWorkspaceStore::new(database.clone())),
            apps: Arc::new(zupflow_sqlite::SqliteAppStore::new(database.clone())),
            installations: Arc::new(zupflow_sqlite::SqliteInstallationStore::new(database.clone())),
            agents: Arc::new(zupflow_sqlite::SqliteAgentStore::new(database.clone())),
            policies: Arc::new(zupflow_sqlite::SqliteToolPolicyStore::new(database.clone())),
            runs: Arc::new(zupflow_sqlite::SqliteRunStore::new(database.clone())),
            provider,
            approvals: Arc::new(zupflow_basic::configured_policy::ConfiguredPolicy::new(Arc::new(
                zupflow_sqlite::SqliteToolPolicyStore::new(database.clone()),
            ))),
            events: Arc::new(zupflow_basic::in_process_events::InProcessEventBus::default()),
            metrics: Arc::new(zupflow_basic::in_process_metrics::InProcessMetrics::default()),
            model_store: Arc::new(zupflow_sqlite::SqliteModelStore::new(database.clone())),
            model_catalog,
            connect: Arc::new(zupflow_sqlite::SqliteConnectStore::new(database.clone())),
            channels: Arc::new(zupflow_sqlite::SqliteChannelStore::new(database.clone())),
            channel_provider: std::sync::Arc::new(zupflow_channels::fake::FakeChannelProvider::default()),
            channel_provisioner: Arc::new(
                zupflow_basic::scripted_channel_provisioner::ScriptedChannelProvisioner::default(),
            ),
            jobs: Arc::new(zupflow_sqlite::SqliteJobQueue::new(database.clone())),
            tools: Arc::new(zupflow_sqlite::SqliteToolRegistry::new(database.clone())),
            cache: Arc::new(zupflow_basic::memory_cache::MemoryCache::default()),
            keys: Arc::new(zupflow_basic::local_keys::LocalKeyService::from_server_secret(&[7; 32])),
            outbound: providers.clone(),
            public_url: "http://localhost:8080".to_owned(),
            sign_in: zupflow_server::sign_in::SignInSettings::Local,
            api_keys: Arc::new(SqliteApiKeyStore::new(database.clone())),
            webhooks: Arc::new(zupflow_sqlite::SqliteWebhookStore::new(database.clone())),
            audit: Arc::new(zupflow_sqlite::SqliteAuditStore::new(database.clone())),
            idempotency: Arc::new(zupflow_sqlite::SqliteIdempotencyStore::new(database.clone())),
            fingerprinter: Arc::new(zupflow_core::idempotency::Fingerprinter::new([7; 32])),
            api_key_hasher: Arc::new(zupflow_core::api_keys::ApiKeyHasher::new([7; 32])),
        };
        Self {
            app: zupflow_server::app(state.clone(), RequestGuard::default()),
            state,
            directory,
            providers,
            models,
            database,
        }
    }

    pub async fn send(&self, call: Call<'_>) -> (StatusCode, Value) {
        send_to(&self.app, call).await
    }
}

/// Sends `call` to `app`: the status and the JSON body, if any.
pub async fn send_to(app: &axum::Router, call: Call<'_>) -> (StatusCode, Value) {
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
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, json)
}

impl Server {
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
            serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8(bytes.to_vec()).unwrap()))
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

    /// Connects OpenAI in `org`'s `environment` with [`PROVIDER_KEY`], adds
    /// [`DEFAULT_MODEL`] from it and makes it the environment's default, so
    /// runs there have a model. Returns the model's ID.
    pub async fn default_model(&self, user: &str, org: &str, environment: &str) -> String {
        let (status, connection) = self
            .send(
                call(user, Method::POST, format!("/v1/organizations/{org}/model-connections")).body(json!({
                    "environment": environment, "kind": "openai", "label": "OpenAI", "api_key": PROVIDER_KEY,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{connection}");
        let (status, model) = self
            .send(
                call(user, Method::POST, format!("/v1/organizations/{org}/models")).body(json!({
                    "connection_id": connection["data"]["id"], "provider_model_id": DEFAULT_MODEL,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{model}");
        let model_id = model["data"]["id"].as_str().unwrap().to_owned();
        let (status, body) = self
            .send(
                call(
                    user,
                    Method::PUT,
                    format!("/v1/organizations/{org}/default-models/{environment}"),
                )
                .body(json!({ "model_id": model_id })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        model_id
    }

    /// Makes `user` an active member of `org` with `role`: invited by `owner`,
    /// and accepted.
    pub async fn member(&self, owner: &str, org: &str, user: &str, role: &str) {
        self.send(call(user, Method::GET, "/v1/me")).await;
        let (status, invitation) = self
            .send(
                call(owner, Method::POST, format!("/v1/organizations/{org}/members"))
                    .body(json!({ "email": format!("{user}@people.example"), "role": role })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED);
        let id = invitation["data"]["id"].as_str().unwrap();
        let (status, joined) = self
            .send(call(user, Method::POST, format!("/v1/me/invitations/{id}/accept")))
            .await;
        assert_eq!(status, StatusCode::OK, "{joined}");
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
        client: None,
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
