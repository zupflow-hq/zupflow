//! `OidcIdentity` against a provider served on loopback: discovery, the key
//! set, rotation and outages, and every way a token must be refused.

#![allow(clippy::unwrap_used)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{Value, json};
use zupflow_core::identity::{Identity, IdentityError};
use zupflow_oidc::{OidcConfig, OidcConfigError, OidcIdentity};

const AUDIENCE: &str = "console-client";

/// A P-256 signing key with its public JWK.
struct SigningKey {
    kid: String,
    encoding: EncodingKey,
    jwk: Value,
}

impl SigningKey {
    fn new(kid: &str) -> Self {
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new()).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref()).unwrap();
        let point = pair.public_key().as_ref().to_vec();
        Self {
            kid: kid.to_owned(),
            encoding: EncodingKey::from_ec_der(pkcs8.as_ref()),
            jwk: json!({
                "kty": "EC", "crv": "P-256", "use": "sig", "alg": "ES256", "kid": kid,
                "x": URL_SAFE_NO_PAD.encode(&point[1..33]),
                "y": URL_SAFE_NO_PAD.encode(&point[33..65]),
            }),
        }
    }

    fn sign(&self, claims: &Value) -> String {
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(self.kid.clone());
        jsonwebtoken::encode(&header, claims, &self.encoding).unwrap()
    }
}

struct ProviderState {
    issuer: String,
    discovery_issuer: Mutex<String>,
    /// The `jwks_uri` discovery names, if not the provider's own.
    jwks_uri: Mutex<Option<String>>,
    keys: Mutex<Vec<Value>>,
    up: AtomicBool,
    /// The key set answers with a redirect to itself instead.
    redirect: AtomicBool,
    /// The key set answers with 300 KiB of padding around the keys.
    oversize: AtomicBool,
    key_fetches: AtomicUsize,
}

async fn discovery(State(state): State<Arc<ProviderState>>) -> Response {
    if !state.up.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let issuer = state.discovery_issuer.lock().unwrap().clone();
    let jwks_uri = state
        .jwks_uri
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| format!("{}/jwks", state.issuer));
    Json(json!({ "issuer": issuer, "jwks_uri": jwks_uri })).into_response()
}

async fn jwks(State(state): State<Arc<ProviderState>>) -> Response {
    state.key_fetches.fetch_add(1, Ordering::SeqCst);
    if !state.up.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if state.redirect.load(Ordering::SeqCst) {
        // To a copy that works, so a client following redirects would succeed.
        return (
            StatusCode::FOUND,
            [("Location", format!("{}/jwks-moved", state.issuer))],
        )
            .into_response();
    }
    let keys = state.keys.lock().unwrap().clone();
    if state.oversize.load(Ordering::SeqCst) {
        return Json(json!({ "keys": keys, "padding": "x".repeat(300 << 10) })).into_response();
    }
    Json(json!({ "keys": keys })).into_response()
}

struct Provider {
    state: Arc<ProviderState>,
    key: SigningKey,
}

impl Provider {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let key = SigningKey::new("key-1");
        let state = Arc::new(ProviderState {
            issuer: issuer.clone(),
            discovery_issuer: Mutex::new(issuer),
            jwks_uri: Mutex::new(None),
            keys: Mutex::new(vec![key.jwk.clone()]),
            up: AtomicBool::new(true),
            redirect: AtomicBool::new(false),
            oversize: AtomicBool::new(false),
            key_fetches: AtomicUsize::new(0),
        });
        let app = axum::Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .route(
                "/jwks-moved",
                get(|State(state): State<Arc<ProviderState>>| async move {
                    Json(json!({ "keys": state.keys.lock().unwrap().clone() }))
                }),
            )
            .with_state(state.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { state, key }
    }

    fn issuer(&self) -> String {
        self.state.issuer.clone()
    }

    fn identity(&self) -> OidcIdentity {
        OidcIdentity::new(OidcConfig {
            issuer: self.issuer(),
            audience: AUDIENCE.to_owned(),
            jwks_url: None,
        })
        .unwrap()
    }

    fn fetches(&self) -> usize {
        self.state.key_fetches.load(Ordering::SeqCst)
    }

    fn claims(&self) -> Value {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        json!({
            "iss": self.issuer(), "aud": AUDIENCE, "sub": "person-1",
            "iat": now, "exp": now + 300,
            "email": "Person@People.Example", "email_verified": true, "name": "A Person",
            "picture": "https://images.people.example/a.png",
        })
    }

    fn token(&self) -> String {
        self.key.sign(&self.claims())
    }

    fn token_with(&self, changes: Value) -> String {
        let mut claims = self.claims();
        for (name, value) in changes.as_object().unwrap() {
            if value.is_null() {
                claims.as_object_mut().unwrap().remove(name);
            } else {
                claims[name] = value.clone();
            }
        }
        self.key.sign(&claims)
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

#[tokio::test]
async fn vouches_for_a_valid_token() {
    let provider = Provider::start().await;
    let identity = provider.identity();
    let verified = identity.verify(Some(&provider.token())).await.unwrap();
    assert_eq!(verified.provider, provider.issuer());
    assert_eq!(verified.subject, "person-1");
    assert_eq!(verified.profile.email, "person@people.example", "lowercased");
    assert!(verified.profile.email_verified);
    assert_eq!(verified.profile.name, "A Person");
    assert_eq!(
        verified.profile.avatar_url.as_deref(),
        Some("https://images.people.example/a.png")
    );

    // An audience list that includes ours, and the string "true" some providers send.
    let token = provider.token_with(json!({ "aud": ["other", AUDIENCE], "email_verified": "true" }));
    assert!(identity.verify(Some(&token)).await.unwrap().profile.email_verified);
    let token = provider.token_with(json!({ "email_verified": null, "picture": "http://insecure.example/a.png" }));
    let verified = identity.verify(Some(&token)).await.unwrap();
    assert!(!verified.profile.email_verified, "missing means unverified");
    assert_eq!(verified.profile.avatar_url, None, "only https pictures");

    // Within the minute's leeway.
    let token = provider.token_with(json!({ "exp": now() - 30, "nbf": now() + 30 }));
    assert!(
        identity.verify(Some(&token)).await.is_ok(),
        "clock skew within the leeway"
    );

    // OIDC Core §10.1: with one key in the set, a token may leave out `kid`.
    let token = jsonwebtoken::encode(
        &Header::new(Algorithm::ES256),
        &provider.claims(),
        &provider.key.encoding,
    )
    .unwrap();
    assert!(identity.verify(Some(&token)).await.is_ok(), "no kid");

    assert_eq!(provider.fetches(), 1, "keys are cached");
}

#[tokio::test]
async fn refuses_every_token_it_should() {
    let provider = Provider::start().await;
    let identity = provider.identity();
    let stranger = SigningKey::new("key-1");
    let header_only = |header: Value, claims: &Value| {
        format!(
            "{}.{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string()),
            URL_SAFE_NO_PAD.encode(b"signature")
        )
    };
    let hmac_with_public_key = {
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("key-1".to_owned());
        let secret = provider.key.jwk.to_string();
        jsonwebtoken::encode(
            &header,
            &provider.claims(),
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap()
    };
    let refused = [
        ("garbage", "not.a.token".to_owned()),
        (
            "the wrong issuer",
            provider.token_with(json!({ "iss": "https://elsewhere.example" })),
        ),
        (
            "the wrong audience",
            provider.token_with(json!({ "aud": "another-client" })),
        ),
        (
            "expired beyond the minute's leeway",
            provider.token_with(json!({ "exp": now() - 90 })),
        ),
        ("not yet valid", provider.token_with(json!({ "nbf": now() + 600 }))),
        ("no expiry", provider.token_with(json!({ "exp": null }))),
        ("no subject", provider.token_with(json!({ "sub": null }))),
        ("no email", provider.token_with(json!({ "email": null }))),
        ("a blank email", provider.token_with(json!({ "email": "   " }))),
        (
            "an email longer than a user can have",
            provider.token_with(json!({ "email": format!("{}@people.example", "a".repeat(320)) })),
        ),
        (
            "a subject longer than 300",
            provider.token_with(json!({ "sub": "s".repeat(301) })),
        ),
        ("a stranger's key under our key ID", stranger.sign(&provider.claims())),
        ("an unknown key ID", SigningKey::new("key-9").sign(&provider.claims())),
        (
            "alg none",
            header_only(json!({ "alg": "none", "kid": "key-1" }), &provider.claims()),
        ),
        ("HS256 keyed with the public key", hmac_with_public_key),
        (
            "RS256 against an EC key",
            header_only(json!({ "alg": "RS256", "kid": "key-1" }), &provider.claims()),
        ),
    ];
    for (what, token) in refused {
        assert_eq!(
            identity.verify(Some(&token)).await,
            Err(IdentityError::Unauthenticated),
            "{what}"
        );
    }
    assert_eq!(
        identity.verify(None).await,
        Err(IdentityError::Unauthenticated),
        "no token"
    );
    assert!(
        identity.verify(Some(&provider.token())).await.is_ok(),
        "and a good one still passes"
    );
}

#[tokio::test]
async fn follows_a_key_rotation_without_letting_unknown_keys_hammer_the_provider() {
    let provider = Provider::start().await;
    let identity = provider
        .identity()
        .with_timing(Duration::from_secs(3600), Duration::from_millis(300));
    identity.verify(Some(&provider.token())).await.unwrap();
    assert_eq!(provider.fetches(), 1);

    // Tokens naming made-up keys: at most one refetch per interval.
    for n in 0..20 {
        let token = SigningKey::new(&format!("made-up-{n}")).sign(&provider.claims());
        assert_eq!(identity.verify(Some(&token)).await, Err(IdentityError::Unauthenticated));
    }
    assert!(provider.fetches() <= 2, "{} fetches", provider.fetches());

    // The provider rotates; a token with the new key is accepted once allowed to refetch.
    let rotated = SigningKey::new("key-2");
    provider.state.keys.lock().unwrap().push(rotated.jwk.clone());
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(identity.verify(Some(&rotated.sign(&provider.claims()))).await.is_ok());
}

#[tokio::test]
async fn keeps_working_on_cached_keys_while_the_provider_is_down() {
    let provider = Provider::start().await;
    let identity = provider
        .identity()
        .with_timing(Duration::from_millis(200), Duration::from_millis(100));
    provider.state.up.store(false, Ordering::SeqCst);
    for attempt in ["first", "second, inside the refresh limit"] {
        assert_eq!(
            identity.verify(Some(&provider.token())).await,
            Err(IdentityError::Unavailable),
            "{attempt}: no keys yet and the provider is down is an outage, not a bad token"
        );
    }

    provider.state.up.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(150)).await;
    identity.verify(Some(&provider.token())).await.unwrap();
    provider.state.up.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        identity.verify(Some(&provider.token())).await.is_ok(),
        "stale keys still verify when a refresh fails"
    );
}

#[tokio::test]
async fn trusts_discovery_only_for_its_own_issuer() {
    let provider = Provider::start().await;
    *provider.state.discovery_issuer.lock().unwrap() = "https://impostor.example".to_owned();
    assert_eq!(
        provider.identity().verify(Some(&provider.token())).await,
        Err(IdentityError::Unavailable)
    );
}

#[test]
fn refuses_insecure_configuration() {
    let config = |issuer: &str, audience: &str, jwks: Option<&str>| OidcConfig {
        issuer: issuer.to_owned(),
        audience: audience.to_owned(),
        jwks_url: jwks.map(str::to_owned),
    };
    let refused = [
        (
            config("http://idp.example", AUDIENCE, None),
            OidcConfigError::InvalidIssuer,
        ),
        (config("not a url", AUDIENCE, None), OidcConfigError::InvalidIssuer),
        (
            config("https://user:pw@idp.example", AUDIENCE, None),
            OidcConfigError::InvalidIssuer,
        ),
        (config("https://idp.example", " ", None), OidcConfigError::EmptyAudience),
        (
            config(&format!("https://idp.example/{}", "x".repeat(300)), AUDIENCE, None),
            OidcConfigError::InvalidIssuer,
        ),
        (
            config("https://idp.example", AUDIENCE, Some("http://keys.example/jwks")),
            OidcConfigError::InvalidJwksUrl,
        ),
    ];
    for (config, expected) in refused {
        assert_eq!(OidcIdentity::new(config).err(), Some(expected));
    }
    for issuer in [
        "https://idp.example",
        "http://localhost:8081",
        "http://127.0.0.1:9000/realms/x",
    ] {
        assert!(OidcIdentity::new(config(issuer, AUDIENCE, None)).is_ok(), "{issuer}");
    }
}

/// An RSA key: the most common provider default.
struct RsaKey {
    encoding: EncodingKey,
    jwk: Value,
}

/// The value of the DER element at the start of `input`, and what follows it.
fn der_element(input: &[u8]) -> (u8, &[u8], &[u8]) {
    let tag = input[0];
    let (length, header) = match input[1] {
        short if short < 0x80 => (usize::from(short), 2),
        long => {
            let bytes = usize::from(long & 0x7f);
            let length = input[2..2 + bytes].iter().fold(0, |n, b| (n << 8) | usize::from(*b));
            (length, 2 + bytes)
        }
    };
    (tag, &input[header..header + length], &input[header + length..])
}

impl RsaKey {
    fn new(kid: &str, published_for: &str) -> Self {
        use aws_lc_rs::encoding::AsDer;
        let pair = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap();
        let pkcs8 = pair.as_der().unwrap();
        // PKCS #8 wraps the PKCS #1 key jsonwebtoken signs with: skip the
        // version and the algorithm, and take the octet string.
        let (_, info, _) = der_element(pkcs8.as_ref());
        let (_, _, rest) = der_element(info);
        let (_, _, rest) = der_element(rest);
        let (tag, pkcs1, _) = der_element(rest);
        assert_eq!(tag, 0x04);
        let public = pair.public_key();
        Self {
            encoding: EncodingKey::from_rsa_der(pkcs1),
            jwk: json!({
                "kty": "RSA", "use": "sig", "alg": published_for, "kid": kid,
                "n": URL_SAFE_NO_PAD.encode(public.modulus().big_endian_without_leading_zero()),
                "e": URL_SAFE_NO_PAD.encode(public.exponent().big_endian_without_leading_zero()),
            }),
        }
    }

    fn sign(&self, algorithm: Algorithm, kid: &str, claims: &Value) -> String {
        let mut header = Header::new(algorithm);
        header.kid = Some(kid.to_owned());
        jsonwebtoken::encode(&header, claims, &self.encoding).unwrap()
    }
}

#[tokio::test]
async fn verifies_rsa_keys_only_for_the_algorithm_they_are_published_for() {
    let provider = Provider::start().await;
    let rsa = RsaKey::new("rsa-1", "RS256");
    provider.state.keys.lock().unwrap().push(rsa.jwk.clone());
    let identity = provider.identity();
    let rs256 = rsa.sign(Algorithm::RS256, "rsa-1", &provider.claims());
    assert!(identity.verify(Some(&rs256)).await.is_ok());
    let ps256 = rsa.sign(Algorithm::PS256, "rsa-1", &provider.claims());
    assert_eq!(
        identity.verify(Some(&ps256)).await,
        Err(IdentityError::Unauthenticated),
        "a key published for RS256 doesn't verify PS256"
    );
}

#[tokio::test]
async fn skips_keys_not_meant_for_signatures() {
    let provider = Provider::start().await;
    let mut jwk = provider.key.jwk.clone();
    jwk["use"] = json!("enc");
    *provider.state.keys.lock().unwrap() = vec![jwk];
    assert_eq!(
        provider.identity().verify(Some(&provider.token())).await,
        Err(IdentityError::Unauthenticated)
    );
}

/// The fetches 0028 bounds: no redirects, at most 256 KiB, and a key set
/// URL from discovery that is HTTPS (or loopback).
#[tokio::test]
async fn fetches_only_what_it_should() {
    let provider = Provider::start().await;
    provider.state.redirect.store(true, Ordering::SeqCst);
    assert_eq!(
        provider.identity().verify(Some(&provider.token())).await,
        Err(IdentityError::Unavailable),
        "a redirect isn't followed"
    );
    provider.state.redirect.store(false, Ordering::SeqCst);

    provider.state.oversize.store(true, Ordering::SeqCst);
    assert_eq!(
        provider.identity().verify(Some(&provider.token())).await,
        Err(IdentityError::Unavailable),
        "more than 256 KiB isn't read"
    );
    provider.state.oversize.store(false, Ordering::SeqCst);

    // 0.0.0.0 reaches this provider but isn't a loopback name, so plain HTTP
    // to it must be refused even though the fetch would work.
    let port = provider.issuer().rsplit(':').next().unwrap().to_owned();
    *provider.state.jwks_uri.lock().unwrap() = Some(format!("http://0.0.0.0:{port}/jwks-moved"));
    assert_eq!(
        provider.identity().verify(Some(&provider.token())).await,
        Err(IdentityError::Unavailable),
        "a plain-HTTP key set off loopback isn't fetched"
    );
    *provider.state.jwks_uri.lock().unwrap() = None;
    assert!(provider.identity().verify(Some(&provider.token())).await.is_ok());
}

/// Issues the provider's tokens for the shared `Identity` suite.
struct ProviderCredentials<'a>(&'a Provider);

impl zupflow_conformance::identity::Credentials for ProviderCredentials<'_> {
    fn issue(&self, subject: &str, email: &str, email_verified: bool) -> String {
        self.0
            .token_with(json!({ "sub": subject, "email": email, "email_verified": email_verified }))
    }

    fn forged(&self) -> Vec<(String, String)> {
        vec![
            (
                "a stranger's key".to_owned(),
                SigningKey::new("key-1").sign(&self.0.claims()),
            ),
            (
                "another audience".to_owned(),
                self.0.token_with(json!({ "aud": "elsewhere" })),
            ),
            ("expired".to_owned(), self.0.token_with(json!({ "exp": now() - 600 }))),
            ("not a token".to_owned(), "garbage".to_owned()),
        ]
    }
}

#[tokio::test]
async fn conforms() {
    let provider = Provider::start().await;
    zupflow_conformance::identity::run(&provider.identity(), &ProviderCredentials(&provider)).await;
}

/// A provider that leaves out `kid` rotates its only key: the next sign-in
/// looks again instead of waiting out the cache.
#[tokio::test]
async fn follows_a_rotation_by_a_provider_that_leaves_out_kid() {
    let provider = Provider::start().await;
    let identity = provider
        .identity()
        .with_timing(Duration::from_secs(3600), Duration::from_millis(100));
    let unnamed = |key: &SigningKey| {
        jsonwebtoken::encode(&Header::new(Algorithm::ES256), &provider.claims(), &key.encoding).unwrap()
    };
    assert!(identity.verify(Some(&unnamed(&provider.key))).await.is_ok());
    let next = SigningKey::new("next");
    *provider.state.keys.lock().unwrap() = vec![next.jwk.clone()];
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        identity.verify(Some(&unnamed(&next))).await.is_ok(),
        "the rotated key is fetched"
    );
}

/// Keys are trusted for the cache's lifetime only: a key the provider has
/// withdrawn stops verifying once the cache expires.
#[tokio::test]
async fn stops_trusting_a_withdrawn_key_when_the_cache_expires() {
    let provider = Provider::start().await;
    let identity = provider
        .identity()
        .with_timing(Duration::from_millis(200), Duration::from_millis(50));
    assert!(identity.verify(Some(&provider.token())).await.is_ok());
    let replacement = SigningKey::new("replacement");
    *provider.state.keys.lock().unwrap() = vec![replacement.jwk.clone()];
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        identity.verify(Some(&provider.token())).await,
        Err(IdentityError::Unauthenticated),
        "the withdrawn key no longer verifies"
    );
}
