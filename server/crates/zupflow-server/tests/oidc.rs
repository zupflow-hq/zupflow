//! The server with `oidc` sign-in, end to end: a provider on loopback, real
//! signed ID tokens, and the first-sign-in rule from decision 0027.

#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use axum::Json;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http_body_util::BodyExt;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{Value, json};
use tower::ServiceExt;
use zupflow_oidc::{OidcConfig, OidcIdentity};
use zupflow_server::{AppState, RequestGuard};
use zupflow_sqlite::{Database, SqliteApiKeyStore, SqliteOrganizationStore, SqliteWorkspaceStore};

struct Setup {
    app: axum::Router,
    issuer: String,
    key: EncodingKey,
    directory: PathBuf,
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

async fn setup() -> Setup {
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new()).unwrap();
    let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref()).unwrap();
    let point = pair.public_key().as_ref().to_vec();
    let jwk = json!({
        "kty": "EC", "crv": "P-256", "use": "sig", "kid": "k1",
        "x": URL_SAFE_NO_PAD.encode(&point[1..33]), "y": URL_SAFE_NO_PAD.encode(&point[33..65]),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let discovery = json!({ "issuer": issuer, "jwks_uri": format!("{issuer}/jwks") });
    let provider = axum::Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(move || async move { Json(discovery) }),
        )
        .route("/jwks", get(move || async move { Json(json!({ "keys": [jwk] })) }));
    tokio::spawn(async move { axum::serve(listener, provider).await });

    let directory = std::env::temp_dir().join(format!("zupflow-oidc-{}", uuid::Uuid::now_v7().simple()));
    std::fs::create_dir(&directory).unwrap();
    let database = Database::open(&directory.join("zupflow.sqlite")).await.unwrap();
    let identity = OidcIdentity::new(OidcConfig {
        issuer: issuer.clone(),
        audience: "console".to_owned(),
        jwks_url: None,
    })
    .unwrap();
    let state = AppState {
        identity: Arc::new(identity),
        organizations: Arc::new(SqliteOrganizationStore::new(database.clone())),
        workspaces: Arc::new(SqliteWorkspaceStore::new(database.clone())),
        apps: Arc::new(zupflow_sqlite::SqliteAppStore::new(database.clone())),
        installations: Arc::new(zupflow_sqlite::SqliteInstallationStore::new(database.clone())),
        agents: Arc::new(zupflow_sqlite::SqliteAgentStore::new(database.clone())),
        models: Arc::new(zupflow_basic::configured_models::ConfiguredModels::none()),
        connect: Arc::new(zupflow_sqlite::SqliteConnectStore::new(database.clone())),
        jobs: Arc::new(zupflow_sqlite::SqliteJobQueue::new(database.clone())),
        tools: Arc::new(zupflow_sqlite::SqliteToolRegistry::new(database.clone())),
        cache: Arc::new(zupflow_basic::memory_cache::MemoryCache::default()),
        keys: Arc::new(zupflow_basic::local_keys::LocalKeyService::from_server_secret(&[7; 32])),
        outbound: Arc::new(zupflow_outbound::SafeHttpClient::new().unwrap()),
        public_url: "http://localhost:8080".to_owned(),
        api_keys: Arc::new(SqliteApiKeyStore::new(database.clone())),
        idempotency: Arc::new(zupflow_sqlite::SqliteIdempotencyStore::new(database)),
        fingerprinter: Arc::new(zupflow_core::idempotency::Fingerprinter::new([7; 32])),
        api_key_hasher: Arc::new(zupflow_core::api_keys::ApiKeyHasher::new([7; 32])),
    };
    Setup {
        app: zupflow_server::app(
            state,
            RequestGuard::with_origins(&["https://console.people.example".to_owned()]),
        ),
        issuer,
        key: EncodingKey::from_ec_der(pkcs8.as_ref()),
        directory,
    }
}

impl Setup {
    fn token(&self, subject: &str, email: &str, verified: bool) -> String {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let claims = json!({
            "iss": self.issuer, "aud": "console", "sub": subject, "exp": now + 300,
            "email": email, "email_verified": verified, "name": subject,
        });
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some("k1".to_owned());
        jsonwebtoken::encode(&header, &claims, &self.key).unwrap()
    }

    async fn me(&self, token: Option<&str>) -> (StatusCode, Value) {
        self.me_from(token, None).await
    }

    async fn me_from(&self, token: Option<&str>, origin: Option<&str>) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .uri("/v1/me")
            .header("Host", "zupflow.people.example");
        if let Some(origin) = origin {
            request = request.header("Origin", origin);
        }
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
}

#[tokio::test]
async fn signs_in_with_a_verified_id_token() {
    let setup = setup().await;
    let (status, body) = setup.me(None).await;
    assert_eq!(
        (status, &body["error"]["code"]),
        (StatusCode::UNAUTHORIZED, &json!("unauthenticated"))
    );

    let (status, body) = setup
        .me(Some(&setup.token("unverified", "squatter@people.example", false)))
        .await;
    assert_eq!(
        (status, &body["error"]["code"]),
        (StatusCode::UNAUTHORIZED, &json!("email_unverified"))
    );

    let token = setup.token("person-1", "Person@People.Example", true);
    let (status, body) = setup.me(Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["user"]["email"], "person@people.example");
    let user = body["data"]["user"]["id"].clone();
    assert_eq!(
        setup.me(Some(&token)).await.1["data"]["user"]["id"],
        user,
        "the same user each time"
    );

    let (status, body) = setup
        .me(Some(&setup.token("person-2", "person@people.example", true)))
        .await;
    assert_eq!(
        (status, &body["error"]["code"]),
        (StatusCode::UNAUTHORIZED, &json!("email_in_use")),
        "another identity can't take over the user"
    );
}

/// With `oidc` the server answers any Host, but a browser request still has
/// to come from an allowed origin.
#[tokio::test]
async fn answers_any_host_but_only_allowed_origins() {
    let setup = setup().await;
    let token = setup.token("person-1", "person@people.example", true);
    let (status, _) = setup
        .me_from(Some(&token), Some("https://console.people.example"))
        .await;
    assert_eq!(status, StatusCode::OK);
    for origin in ["https://attacker.example", "null", "http://console.people.example"] {
        let (status, body) = setup.me_from(Some(&token), Some(origin)).await;
        assert_eq!(
            (status, &body["error"]["code"]),
            (StatusCode::FORBIDDEN, &json!("origin_not_allowed")),
            "{origin}"
        );
    }
}
