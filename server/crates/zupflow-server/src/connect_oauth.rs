//! OAuth for hosted Connect (decision 0037): the hosted page starts an OAuth
//! app's consent, and the provider sends the browser back to the callback,
//! which exchanges the code and completes the session.
//!
//! The start makes a fresh `state` per attempt and keeps only its hash; what
//! the callback needs (the PKCE verifier and the link's reference) is sealed
//! under the attempt's own context. The callback finds the attempt by its
//! `state`'s hash and spends it in the same statement, so a `state` works
//! once; everything after uses the session's own organization and workspace.
//! The callback answers a redirect to the hosted page, which shows what came
//! of it: the session completed, or failed with a code.
//!
//! An `mcp_oauth` app (decision 0039) has no endpoints or client of its own:
//! each start discovers the MCP server's authorization server, and uses the
//! client registered there for this app and environment (a workspace app's,
//! for its workspace), registering one on first use. PKCE is always used, and
//! both requests name the MCP server as the `resource`. What the start
//! discovered travels to the callback sealed with the verifier.

use std::collections::BTreeMap;

use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;
use zupflow_core::apps::{
    App, AppError, AuthTemplate, AuthorizationServer, ClientAuthentication, ClientRegistration, ClientRegistrationId,
    Configuration, NewClientRegistration, OAuthScope,
};
use zupflow_core::connect::{
    Authorization, Completion, ConnectError, ConnectIntent, ConnectSession, ConnectSessionStatus, OAuthState,
    Reference, ReferenceHash,
};
use zupflow_core::error::ApiError;
use zupflow_core::ids::InstallationId;
use zupflow_core::installations::{ConnectedAccount, DiscoveredClient, InstalledApp, NewCredential, TemplateSnapshot};
use zupflow_core::keys::{self, EncryptionContext};
use zupflow_core::outbound::{Credential, Limits, Method, OutboundRequest};
use zupflow_core::time::Timestamp;
use zupflow_mcp::sign_in::{self, SignInError};

use crate::app_configurations::{check_public_https, redirect_uri};
use crate::auth::AppState;
use crate::auth_templates::same_origin;
use crate::connect::{attributed, completion_audit, connect_failure, not_pending, still_eligible};
use crate::http::{Failure, envelope};

/// What the start answers: the contract's `AuthorizeConnectResult`.
#[derive(Serialize)]
struct AuthorizeView {
    authorization_url: String,
}

/// What the callback needs back, sealed while the provider has the browser.
#[derive(Serialize)]
struct Sealing<'a> {
    reference: &'a str,
    verifier: Option<&'a str>,
    /// For an `mcp_oauth` app: where the start signed in, and as whom.
    client: Option<&'a DiscoveredClient>,
    /// A configured OAuth app's exact trusted issuer at authorization start.
    issuer: Option<&'a str>,
}

/// [`Sealing`], opened.
#[derive(Deserialize)]
struct Pending {
    reference: String,
    verifier: Option<String>,
    #[serde(default)]
    client: Option<DiscoveredClient>,
    #[serde(default)]
    issuer: Option<String>,
}

/// What a start sends the browser to, and as which client.
struct Consent {
    authorize_url: String,
    client_id: String,
    pkce: bool,
    /// For an `mcp_oauth` app: its server, the `resource` both requests name.
    client: Option<DiscoveredClient>,
    issuer: Option<String>,
}

/// A PKCE verifier: two references, 64 base62 characters (RFC 7636 asks for
/// 43 to 128 from this alphabet's superset).
fn verifier() -> Result<Zeroizing<String>, Failure> {
    let first = Reference::generate().map_err(connect_failure)?;
    let second = Reference::generate().map_err(connect_failure)?;
    Ok(Zeroizing::new(format!("{}{}", first.expose(), second.expose())))
}

/// The `S256` challenge for a verifier.
fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// An app session's app. A channel session never reaches the app's OAuth
/// code: its own routes answer for it.
fn installed(session: &ConnectSession) -> Result<InstalledApp, Failed> {
    session.app.ok_or(Failed("app_not_eligible"))
}

fn authorization_context(session: &ConnectSession, installed: InstalledApp, attempt: u32) -> EncryptionContext {
    EncryptionContext::connect_authorization(
        session.organization_id,
        session.workspace_id,
        session.environment,
        installed.id(),
        session.id,
        attempt,
    )
}

/// An OAuth app whose template lacks an endpoint can't be connected: its
/// owner sets them in the app's authentication settings.
fn endpoints_missing(app: &App) -> Failure {
    ApiError::new(
        422,
        "configuration_not_ready",
        format!(
            "{} has no authorization or token URL. Set them in the app's authentication settings.",
            app.name
        ),
    )
    .into()
}

/// Starts an OAuth app's consent: a new attempt for the session, with a
/// fresh `state` and, if the app uses PKCE, a fresh verifier. Only the
/// newest attempt's `state` completes the session.
pub async fn authorize(State(state): State<AppState>, Path(reference): Path<String>) -> Result<Response, Failure> {
    let session = attributed(&state, &reference).await?;
    let status = session.status_at(Timestamp::now());
    if status != ConnectSessionStatus::Pending {
        return Err(not_pending(status));
    }
    if session.target.is_some() {
        return crate::channel_install::authorize(&state, &session, &reference).await;
    }
    let (app, configuration) = still_eligible(&state, &session).await?;
    let Some(installed) = session.app else {
        return Err(ApiError::internal().into());
    };
    let consent = match &app.auth {
        AuthTemplate::OAuth2 {
            authorize_url,
            token_url,
            issuer,
            pkce,
            ..
        } => {
            let (Some(authorize_url), Some(_)) = (authorize_url, token_url) else {
                return Err(endpoints_missing(&app));
            };
            let Some(client_id) = &configuration.oauth_client_id else {
                // A ready OAuth configuration has one.
                tracing::error!(event = "connect.oauth_without_client_id", app_id = %app.id);
                return Err(ApiError::internal().into());
            };
            Consent {
                authorize_url: authorize_url.clone(),
                client_id: client_id.clone(),
                pkce: *pkce,
                client: None,
                issuer: issuer.clone(),
            }
        }
        AuthTemplate::McpOAuth { .. } => match signed_in_consent(&state, &session, &app, &configuration).await {
            Ok(consent) => consent,
            Err(Failed(code)) => return Err(failed_start(&state, &session, &app, code).await),
        },
        AuthTemplate::Token { .. } => {
            return Err(ApiError::new(422, "wrong_auth_type", format!("{} connects with a token.", app.name)).into());
        }
    };
    let Some(attempt) = session.authorization_attempt.checked_add(1) else {
        return Err(ApiError::internal().into());
    };

    let oauth_state = OAuthState::generate().map_err(connect_failure)?;
    let verifier = if consent.pkce { Some(verifier()?) } else { None };
    let sealing = Sealing {
        reference: &reference,
        verifier: verifier.as_deref().map(String::as_str),
        client: consent.client.as_ref(),
        issuer: consent.issuer.as_deref(),
    };
    let plaintext = Zeroizing::new(serde_json::to_vec(&sealing).map_err(|_| Failure(ApiError::internal()))?);
    let sealed = keys::seal(
        state.keys.as_ref(),
        &authorization_context(&session, installed, attempt),
        &plaintext,
    )
    .await
    .map_err(|error| {
        tracing::error!(event = "connect.seal_failed", %error);
        connect_failure(ConnectError::Unavailable)
    })?;
    let begun = state
        .connect
        .begin_authorization(
            session.organization_id,
            session.workspace_id,
            session.id,
            &Authorization {
                state_hash: oauth_state.hash(),
                attempt,
                sealed,
            },
            Timestamp::now(),
        )
        .await;
    match begun {
        Ok(()) => {}
        // Finished since it was read: say how, as it is now.
        Err(ConnectError::NotPending) => {
            let now = state
                .connect
                .find_session(session.organization_id, session.workspace_id, session.id)
                .await
                .map_err(connect_failure)?
                .map_or(ConnectSessionStatus::Expired, |session| {
                    session.status_at(Timestamp::now())
                });
            return Err(not_pending(now));
        }
        Err(error) => return Err(connect_failure(error)),
    }

    // Checked when the app saved its template (decision 0035), or by discovery.
    let mut url = url::Url::parse(&consent.authorize_url).map_err(|_| {
        tracing::error!(event = "connect.authorize_url_unparseable", app_id = %app.id);
        Failure(ApiError::internal())
    })?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", &consent.client_id)
            .append_pair("redirect_uri", &redirect_uri(&state.public_url))
            .append_pair("state", oauth_state.expose());
        if !session.requested_scopes.is_empty() {
            query.append_pair("scope", &session.requested_scopes.join(" "));
        }
        if let Some(verifier) = &verifier {
            query
                .append_pair("code_challenge", &challenge(verifier))
                .append_pair("code_challenge_method", "S256");
        }
        // RFC 8707: the token is for this server alone.
        if let Some(client) = &consent.client {
            query.append_pair("resource", &client.resource);
        }
    }
    Ok(envelope(
        StatusCode::OK,
        AuthorizeView {
            authorization_url: url.into(),
        },
    ))
}

// --- Sign-in through the MCP server (decision 0039) -------------------------

/// What a registration was issued beside its client ID, sealed as one value.
#[derive(Serialize)]
struct Issuing<'a> {
    client_secret: Option<&'a str>,
    registration_access_token: Option<&'a str>,
}

/// [`Issuing`], opened.
#[derive(Deserialize)]
struct Issued {
    client_secret: Option<String>,
    registration_access_token: Option<String>,
}

fn registration_context(
    session: &ConnectSession,
    installed: InstalledApp,
    registration: ClientRegistrationId,
) -> EncryptionContext {
    let workspace_id = matches!(installed, InstalledApp::Workspace(_)).then_some(session.workspace_id);
    EncryptionContext::oauth_client_registration(
        session.organization_id,
        workspace_id,
        session.environment,
        installed.id(),
        registration,
    )
}

/// The client registered at `issuer` for the session's app: in its
/// environment for a catalog app, in its workspace for a workspace app.
async fn find_registration(
    state: &AppState,
    session: &ConnectSession,
    issuer: &str,
) -> Result<Option<ClientRegistration>, AppError> {
    let Some(installed) = session.app else {
        return Ok(None);
    };
    match installed {
        InstalledApp::Catalog(id) => {
            state
                .apps
                .find_organization_client_registration(session.organization_id, id, session.environment, issuer)
                .await
        }
        InstalledApp::Workspace(id) => {
            state
                .apps
                .find_workspace_client_registration(session.organization_id, session.workspace_id, id, issuer)
                .await
        }
    }
}

/// Why sign-in through the server can't start: unreachable, or unusable.
/// The server's own words are never logged, only what was wrong.
fn sign_in_failure(app: &App, error: SignInError) -> Failed {
    match error {
        SignInError::Unavailable => {
            tracing::warn!(event = "connect.sign_in_unavailable", app_id = %app.id);
            Failed("provider_unavailable")
        }
        SignInError::Unsupported(why) => {
            tracing::warn!(event = "connect.discovery_failed", app_id = %app.id, why);
            Failed("discovery_failed")
        }
    }
}

/// Finds the MCP server's authorization server, and the client to sign in
/// as there, registering one on first use. Nothing discovered is cached: it
/// is recorded on the registration and travels to the callback sealed.
async fn signed_in_consent(
    state: &AppState,
    session: &ConnectSession,
    app: &App,
    configuration: &Configuration,
) -> Result<Consent, Failed> {
    let Some(resource) = &configuration.mcp_server_url else {
        return Err(Failed("configuration_not_ready"));
    };
    // The template was checked against the server when it was saved; the
    // configuration may name another server since.
    if let AuthTemplate::McpOAuth {
        identity_url: Some(identity_url),
        ..
    } = &app.auth
        && !same_origin(identity_url, resource)
    {
        tracing::warn!(event = "connect.identity_off_server", app_id = %app.id);
        return Err(Failed("identity_off_server"));
    }
    // One deadline for discovery and registration together, so a slow
    // server can't hold the request past its idempotency key's lock.
    let deadline = tokio::time::Instant::now() + sign_in::SIGN_IN_BUDGET;
    let server = sign_in::discover(state.outbound.as_ref(), resource, deadline)
        .await
        .map_err(|error| sign_in_failure(app, error))?;
    // The browser goes there: it must pass what a configured authorization URL does.
    if !public_https(&server.authorization_endpoint) {
        tracing::warn!(event = "connect.discovery_failed", app_id = %app.id, why = "an authorization endpoint on a private host");
        return Err(Failed("discovery_failed"));
    }
    let registration = match find_registration(state, session, &server.issuer).await {
        Ok(Some(registration)) => registration,
        Ok(None) => register(state, session, app, &server, deadline).await?,
        Err(_) => return Err(Failed("unavailable")),
    };
    Ok(Consent {
        authorize_url: server.authorization_endpoint.clone(),
        client_id: registration.client_id.clone(),
        pkce: true,
        issuer: None,
        client: Some(DiscoveredClient {
            issuer: server.issuer,
            response_iss_supported: server.response_iss_supported,
            token_endpoint: server.token_endpoint,
            client_id: registration.client_id,
            resource: resource.clone(),
        }),
    })
}

/// Registers a client at `server` and keeps it, unless a concurrent start
/// kept one first: then that one is used, and this one never is.
async fn register(
    state: &AppState,
    session: &ConnectSession,
    app: &App,
    server: &AuthorizationServer,
    deadline: tokio::time::Instant,
) -> Result<ClientRegistration, Failed> {
    let installed = installed(session)?;
    let registered = sign_in::register(
        state.outbound.as_ref(),
        server,
        &redirect_uri(&state.public_url),
        deadline,
    )
    .await
    .map_err(|error| sign_in_failure(app, error))?;
    let id = ClientRegistrationId::generate();
    let sealed = match (&registered.client_secret, &registered.registration_access_token) {
        (None, None) => None,
        (client_secret, registration_access_token) => {
            let issuing = Issuing {
                client_secret: client_secret.as_deref().map(String::as_str),
                registration_access_token: registration_access_token.as_deref().map(String::as_str),
            };
            let plaintext = Zeroizing::new(serde_json::to_vec(&issuing).map_err(|_| Failed("unavailable"))?);
            let sealed = keys::seal(
                state.keys.as_ref(),
                &registration_context(session, installed, id),
                &plaintext,
            )
            .await
            .map_err(|error| {
                tracing::error!(event = "connect.seal_failed", %error);
                Failed("unavailable")
            })?;
            Some(sealed)
        }
    };
    let registration = NewClientRegistration {
        id,
        server: server.clone(),
        client_id: registered.client_id.clone(),
        authentication: registered.authentication,
        sealed,
    };
    let saved = match installed {
        InstalledApp::Catalog(app_id) => {
            state
                .apps
                .save_organization_client_registration(
                    session.organization_id,
                    app_id,
                    session.environment,
                    &registration,
                )
                .await
        }
        InstalledApp::Workspace(app_id) => {
            state
                .apps
                .save_workspace_client_registration(
                    session.organization_id,
                    session.workspace_id,
                    app_id,
                    &registration,
                )
                .await
        }
    }
    .map_err(|error| match error {
        AppError::NotFound => Failed("app_not_eligible"),
        _ => Failed("unavailable"),
    })?;
    if saved.id != id {
        tracing::info!(event = "connect.registration_kept_another", app_id = %app.id);
    }
    Ok(saved)
}

/// The secret the client the start signed in as authenticates with, opened;
/// `None` for a public client. The registration must still be the one the
/// start used.
async fn registered_secret(
    state: &AppState,
    session: &ConnectSession,
    app: &App,
    client: &DiscoveredClient,
) -> Result<Option<Zeroizing<String>>, Failed> {
    let registration = find_registration(state, session, &client.issuer)
        .await
        .map_err(|_| Failed("unavailable"))?
        .filter(|registration| registration.client_id == client.client_id)
        .ok_or(Failed("configuration_not_ready"))?;
    if registration.authentication == ClientAuthentication::None {
        return Ok(None);
    }
    let Some(sealed) = &registration.sealed else {
        tracing::error!(event = "connect.registration_without_secret", app_id = %app.id);
        return Err(Failed("unavailable"));
    };
    let opened = keys::open(
        state.keys.as_ref(),
        &registration_context(session, installed(session)?, registration.id),
        sealed,
    )
    .await
    .map_err(|error| {
        tracing::error!(event = "connect.registration_unopenable", app_id = %app.id, %error);
        Failed("unavailable")
    })?;
    let issued: Issued = serde_json::from_slice(opened.expose()).map_err(|_| Failed("unavailable"))?;
    // Moved, not copied: each is wiped when it drops.
    let _token = issued.registration_access_token.map(Zeroizing::new);
    issued
        .client_secret
        .map(|secret| Some(Zeroizing::new(secret)))
        .ok_or(Failed("unavailable"))
}

/// A start that couldn't sign in through the MCP server fails its session
/// with `code` when the server is at fault, and says why.
async fn failed_start(state: &AppState, session: &ConnectSession, app: &App, code: &'static str) -> Failure {
    let error = match code {
        "discovery_failed" => ApiError::new(
            502,
            "discovery_failed",
            format!(
                "{}'s server doesn't offer a sign-in Zupflow can use. Ask whoever set up the app to check its server.",
                app.name
            ),
        ),
        "provider_unavailable" => ApiError::new(
            502,
            "provider_unavailable",
            format!("{}'s server couldn't be reached. Start again shortly.", app.name),
        ),
        "app_not_eligible" => ApiError::new(
            422,
            "app_not_eligible",
            "This app is no longer available to this workspace.",
        ),
        "configuration_not_ready" => ApiError::new(
            422,
            "configuration_not_ready",
            format!("{} isn't configured anymore.", app.name),
        ),
        "identity_off_server" => ApiError::new(
            422,
            "identity_off_server",
            format!(
                "{}'s identity URL isn't on its MCP server. Ask whoever set up the app to use an address on the server, or none.",
                app.name
            ),
        ),
        // Our own store or key service: the session can be started again.
        _ => {
            return ApiError::new(
                503,
                "unavailable",
                "The service is unavailable right now. Try again shortly.",
            )
            .into();
        }
    };
    match state
        .connect
        .fail_session(
            session.organization_id,
            session.workspace_id,
            session.id,
            code,
            Timestamp::now(),
        )
        .await
    {
        Ok(_) | Err(ConnectError::NotPending) => {}
        Err(error) => {
            tracing::warn!(event = "connect.fail_session_failed", session_id = %session.id, ?error);
        }
    }
    error.into()
}

/// The `sub` claim of an access token that is a JWT, unverified: it only
/// names an anonymous account, and never authorizes anything.
fn subject(token: &str) -> Option<String> {
    let mut parts = token.split('.');
    let (Some(_), Some(payload), Some(_), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    let claims = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let claims: serde_json::Map<String, Value> = serde_json::from_slice(&claims).ok()?;
    claims
        .get("sub")
        .and_then(Value::as_str)
        .filter(|sub| !sub.is_empty() && sub.len() <= 256)
        .map(str::to_owned)
}

/// Whether the browser may be sent to `url`: the check a configured
/// authorization URL passes when its template is saved.
fn public_https(url: &str) -> bool {
    check_public_https(url, "authorization_endpoint").is_ok()
}

/// An anonymous account's ID: the issuer and subject hashed together, so it
/// names the same account at the same server, and says nothing of either.
fn derived_account(issuer: &str, subject: &str) -> String {
    use std::fmt::Write;
    let digest = Sha256::digest(format!("{issuer}\n{subject}").as_bytes());
    let mut id = String::with_capacity(64);
    for byte in digest {
        let _ = write!(id, "{byte:02x}");
    }
    id
}

// --- The callback -----------------------------------------------------------

/// A callback that names no attempt we hold: no `state`, an unknown one, or
/// one already used. Nothing of any session is said.
fn unknown_state() -> Response {
    crate::connect_page::invalid_oauth_state()
}

/// Back to the hosted page, which reads the session's outcome. Neither the
/// callback's URL (it carries the code) nor the page's (it carries the
/// reference) is sent on as a referrer.
pub(crate) fn back_to(state: &AppState, reference: &str) -> Response {
    let mut response = Redirect::to(&format!("{}/connect/{reference}", state.public_url)).into_response();
    let headers = response.headers_mut();
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Why a callback didn't complete its session: the session's `failure_code`.
pub(crate) struct Failed(pub(crate) &'static str);

/// The provider's answer: the browser returns with `code`, or with `error`.
/// The code is used once, at the token endpoint, and never logged.
pub async fn callback(State(state): State<AppState>, RawQuery(query): RawQuery) -> Response {
    let pairs: Vec<(String, String)> = query
        .as_deref()
        .and_then(|query| serde_urlencoded::from_str(query).ok())
        .unwrap_or_default();
    let mut params = BTreeMap::new();
    for (name, value) in pairs {
        if params.insert(name, value).is_some() {
            return unknown_state();
        }
    }
    let Some(oauth_state) = params
        .get("state")
        .filter(|value| !value.is_empty() && value.len() <= 64)
    else {
        return unknown_state();
    };
    let (session, authorization) = match state.connect.claim_authorization(&ReferenceHash::of(oauth_state)).await {
        Ok(Some(claimed)) => claimed,
        Ok(None) => return unknown_state(),
        Err(error) => return connect_failure(error).into_response(),
    };
    // A channel app's install comes back here too.
    if session.target.is_some() {
        return crate::channel_install::callback(&state, session, authorization, &params).await;
    }
    // Only an app's session begins an authorization.
    let Some(installed) = session.app else {
        return unknown_state();
    };
    let opened = keys::open(
        state.keys.as_ref(),
        &authorization_context(&session, installed, authorization.attempt),
        &authorization.sealed,
    )
    .await;
    let pending: Pending = match opened.map(|plaintext| serde_json::from_slice(plaintext.expose())) {
        Ok(Ok(pending)) => pending,
        _ => {
            tracing::error!(event = "connect.authorization_unopenable", session_id = %session.id);
            return Failure(ApiError::internal()).into_response();
        }
    };
    // Moved, not copied: each buffer is wiped when it drops.
    let reference = Zeroizing::new(pending.reference);
    let verifier = pending.verifier.map(Zeroizing::new);
    let client = pending.client;

    // RFC 9207: bind the response to the issuer before exchanging a code
    // or interpreting an error.
    let issuer_valid = match client.as_ref() {
        Some(client) => match params.get("iss") {
            Some(issuer) => issuer == &client.issuer,
            None => !client.response_iss_supported,
        },
        None => match pending.issuer.as_ref() {
            Some(expected) => params.get("iss") == Some(expected),
            None => !params.contains_key("iss"),
        },
    };
    let outcome = if !issuer_valid {
        Err(Failed("authorization_failed"))
    } else {
        match (params.get("code"), params.get("error")) {
            (_, Some(error)) => {
                // The provider's own words are never passed on, only whether consent was refused.
                tracing::info!(event = "connect.oauth_error", session_id = %session.id, denied = error == "access_denied");
                // Browser-supplied errors can fail this session, but cannot
                // invalidate a registration shared with other workspaces.
                Err(Failed(if error == "access_denied" {
                    "consent_denied"
                } else {
                    "authorization_failed"
                }))
            }
            (Some(code), None) if !code.is_empty() => {
                complete(
                    &state,
                    &session,
                    &Zeroizing::new(code.clone()),
                    verifier.as_deref().map(String::as_str),
                    client.as_ref(),
                )
                .await
            }
            _ => Err(Failed("authorization_failed")),
        }
    };
    if let Err(Failed(failure_code)) = outcome {
        match state
            .connect
            .fail_session(
                session.organization_id,
                session.workspace_id,
                session.id,
                failure_code,
                Timestamp::now(),
            )
            .await
        {
            // Expired or already finished: the page says so.
            Ok(_) | Err(ConnectError::NotPending) => {}
            Err(error) => {
                tracing::warn!(event = "connect.fail_session_failed", session_id = %session.id, ?error);
            }
        }
    }
    back_to(&state, &reference)
}

/// The token endpoint's answer (RFC 6749, section 5.1).
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    pub(crate) token_type: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    pub(crate) expires_in: Option<i64>,
    #[serde(default)]
    pub(crate) scope: Option<String>,
}

/// What an installation's OAuth credential holds, sealed as one value.
#[derive(Serialize)]
pub(crate) struct OAuthCredential<'a> {
    pub(crate) access_token: &'a str,
    pub(crate) refresh_token: Option<&'a str>,
    pub(crate) token_type: Option<&'a str>,
}

/// Exchanges the code, identifies the account, and completes the session
/// with the sealed tokens, revalidating everything the start relied on.
async fn complete(
    state: &AppState,
    session: &ConnectSession,
    code: &str,
    verifier: Option<&str>,
    client: Option<&DiscoveredClient>,
) -> Result<(), Failed> {
    if session.status_at(Timestamp::now()) != ConnectSessionStatus::Pending {
        // Expired while the provider had the browser, or finished another way.
        return Ok(());
    }
    let (app, configuration) = still_eligible(state, session).await.map_err(|Failure(error)| {
        Failed(match error.code {
            "app_not_eligible" => "app_not_eligible",
            "configuration_not_ready" => "configuration_not_ready",
            _ => "unavailable",
        })
    })?;
    let (tokens, identity_url) = match (&app.auth, client) {
        (
            AuthTemplate::OAuth2 {
                token_url: Some(token_url),
                identity_url,
                ..
            },
            None,
        ) => {
            let (client_id, secret) = configured_client(state, session, &app, &configuration).await?;
            let client = TokenClient {
                client_id: &client_id,
                secret: Some(secret.as_str()),
                resource: None,
            };
            (
                exchange(state, &app, token_url, &client, code, verifier).await?,
                identity_url,
            )
        }
        (AuthTemplate::McpOAuth { identity_url, .. }, Some(discovered)) => {
            // The server the start signed in for is still the one configured.
            if configuration.mcp_server_url.as_deref() != Some(discovered.resource.as_str()) {
                return Err(Failed("configuration_not_ready"));
            }
            // Tokens stay with their server: the identity URL too.
            if identity_url
                .as_deref()
                .is_some_and(|identity_url| !same_origin(identity_url, &discovered.resource))
            {
                return Err(Failed("identity_off_server"));
            }
            let secret = registered_secret(state, session, &app, discovered).await?;
            let client = TokenClient {
                client_id: &discovered.client_id,
                secret: secret.as_deref().map(String::as_str),
                resource: Some(&discovered.resource),
            };
            let tokens = match exchange(state, &app, &discovered.token_endpoint, &client, code, verifier).await {
                Err(Failed("invalid_client")) => {
                    crate::oauth_refresh::forget_registration(
                        state,
                        session.organization_id,
                        session.workspace_id,
                        session.environment,
                        installed(session)?,
                        discovered,
                    )
                    .await
                    .map_err(|_| Failed("unavailable"))?;
                    return Err(Failed("token_exchange_failed"));
                }
                result => result?,
            };
            (tokens, identity_url)
        }
        // The template changed since the start.
        _ => return Err(Failed("configuration_not_ready")),
    };
    let identified = match identity_url {
        Some(url) => Some(identify(state, &app, url, &tokens.access_token).await?),
        None => None,
    };
    // Without an identity, an `mcp_oauth` account is anonymous (decision 0039):
    // named from the issuer and the token's subject, if it says one.
    let derived =
        client.and_then(|client| subject(&tokens.access_token).map(|sub| derived_account(&client.issuer, &sub)));
    let anonymous = |installation_id: InstallationId| ConnectedAccount {
        external_id: derived.clone().unwrap_or_else(|| installation_id.to_string()),
        display_name: format!("{} account", app.name),
        email: None,
    };

    let now = Timestamp::now();
    let (installation_id, account, version) = match session.intent {
        ConnectIntent::Create => {
            let installation_id = InstallationId::generate();
            let account = identified.unwrap_or_else(|| anonymous(installation_id));
            (installation_id, account, 1)
        }
        ConnectIntent::Reconnect => {
            let Some(installation_id) = session.installation_id else {
                tracing::error!(event = "connect.reconnect_without_installation", session_id = %session.id);
                return Err(Failed("unavailable"));
            };
            let installation = state
                .installations
                .find_installation(session.organization_id, session.workspace_id, installation_id)
                .await
                .map_err(|_| Failed("unavailable"))?
                .ok_or(Failed("installation_changed"))?;
            // An anonymous account named from a subject, and not after the
            // installation. Only comparable under the same issuer: a subject
            // names an account at its issuer, and a server that moved to
            // another issuer can't say whether its subject is the same person.
            let derived_before = installation.connection.as_ref().and_then(|connection| {
                let anonymous = matches!(
                    &installation.snapshot,
                    Some(TemplateSnapshot {
                        auth: AuthTemplate::McpOAuth { identity_url: None, .. },
                        client: Some(before),
                        ..
                    }) if client.as_ref().is_some_and(|now| now.issuer == before.issuer)
                );
                (anonymous && connection.account.external_id != installation_id.to_string())
                    .then_some(connection.account.external_id.as_str())
            });
            match (installation.connection.clone(), identified) {
                // A reconnect must use the same account (decision 0005).
                (Some(connection), Some(account)) if account.external_id != connection.account.external_id => {
                    return Err(Failed("account_mismatch"));
                }
                // Two anonymous accounts both named from a subject are compared
                // too. Without a subject on either side, nothing can be checked,
                // and the reconnect keeps the account it had.
                (Some(_), None)
                    if derived_before
                        .zip(derived.as_deref())
                        .is_some_and(|(before, now)| before != now) =>
                {
                    return Err(Failed("account_mismatch"));
                }
                (Some(connection), identified) => (
                    installation_id,
                    identified.unwrap_or(connection.account),
                    connection.credential_version + 1,
                ),
                (None, identified) => (
                    installation_id,
                    identified.unwrap_or_else(|| anonymous(installation_id)),
                    1,
                ),
            }
        }
    };

    let context = EncryptionContext::installation_credential(
        session.organization_id,
        session.workspace_id,
        session.environment,
        app.id,
        installation_id,
        version,
    );
    let plaintext = Zeroizing::new(
        serde_json::to_vec(&OAuthCredential {
            access_token: &tokens.access_token,
            refresh_token: tokens.refresh_token.as_deref().map(|token| token.as_str()),
            token_type: tokens.token_type.as_deref(),
        })
        .map_err(|_| Failed("unavailable"))?,
    );
    let sealed = keys::seal(state.keys.as_ref(), &context, &plaintext)
        .await
        .map_err(|error| {
            tracing::error!(event = "connect.seal_failed", %error);
            Failed("unavailable")
        })?;
    let granted_scopes = match &tokens.scope {
        Some(scope) => granted(&app, session, scope),
        None => session.requested_scopes.clone(),
    };
    let expires_at = tokens
        .expires_in
        .filter(|seconds| *seconds >= 0)
        .and_then(|seconds| seconds.checked_mul(1000))
        .and_then(|millis| now.millis().checked_add(millis))
        .map(Timestamp::from_millis);
    let completion = Completion {
        installation_id,
        configuration_version: configuration.version,
        account,
        granted_scopes,
        expires_at,
        credential: NewCredential { sealed, version },
        snapshot: TemplateSnapshot {
            auth: app.auth.clone(),
            app_version: app.version,
            client: client.cloned(),
        },
    };
    match state
        .connect
        .complete_session(
            session.organization_id,
            session.workspace_id,
            session.id,
            &completion,
            Timestamp::now(),
            &completion_audit(&app.name),
        )
        .await
    {
        Ok((_, installation)) => {
            // Best effort: a failure is logged, and the installation is connected either way.
            crate::discovery::enqueue(state, session.organization_id, session.workspace_id, installation.id)
                .await
                .ok();
            Ok(())
        }
        Err(ConnectError::NotPending) => Ok(()),
        Err(ConnectError::InstallationChanged) => Err(Failed("installation_changed")),
        Err(_) => Err(Failed("unavailable")),
    }
}

/// The most scopes an installation records.
const SCOPES_MAX: usize = 50;

/// What the provider says it granted, kept only where it names a scope the
/// session asked for or the app's template offers: the provider may grant
/// less than was asked, but its words are never recorded beyond names we
/// already know.
fn granted(app: &App, session: &ConnectSession, scope: &str) -> Vec<String> {
    let offered: &[OAuthScope] = match &app.auth {
        AuthTemplate::OAuth2 { available_scopes, .. } => available_scopes,
        AuthTemplate::McpOAuth { scopes, .. } => scopes,
        AuthTemplate::Token { .. } => &[],
    };
    let mut granted: Vec<String> = Vec::new();
    for name in scope.split_whitespace() {
        let known = session.requested_scopes.iter().any(|requested| requested == name)
            || offered.iter().any(|offered| offered.name == name);
        if known && !granted.iter().any(|kept| kept == name) && granted.len() < SCOPES_MAX {
            granted.push(name.to_owned());
        }
    }
    granted
}

/// Whether `needle` appears anywhere in `haystack`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|window| window == needle)
}

/// The tokens, kept zeroized until sealed.
pub(crate) struct Tokens {
    pub(crate) access_token: Zeroizing<String>,
    pub(crate) refresh_token: Option<Zeroizing<String>>,
    pub(crate) token_type: Option<String>,
    pub(crate) expires_in: Option<i64>,
    pub(crate) scope: Option<String>,
}

/// The client an exchange authenticates as: with a secret, by HTTP Basic
/// (`client_secret_basic`); without one, as a public client that names
/// itself in the form and proves itself with PKCE.
pub(crate) struct TokenClient<'a> {
    pub(crate) client_id: &'a str,
    pub(crate) secret: Option<&'a str>,
    /// For an `mcp_oauth` app, the MCP server the token is for (RFC 8707).
    pub(crate) resource: Option<&'a str>,
}

/// An OAuth app's own client: its configured ID and secret, opened.
async fn configured_client(
    state: &AppState,
    session: &ConnectSession,
    app: &App,
    configuration: &Configuration,
) -> Result<(String, Zeroizing<String>), Failed> {
    let (Some(client_id), Some(secret)) = (&configuration.oauth_client_id, &configuration.client_secret) else {
        return Err(Failed("configuration_not_ready"));
    };
    let installed = installed(session)?;
    let stored = match installed {
        InstalledApp::Catalog(id) => {
            state
                .apps
                .find_organization_client_secret(session.organization_id, id, session.environment)
                .await
        }
        InstalledApp::Workspace(id) => {
            state
                .apps
                .find_workspace_client_secret(session.organization_id, session.workspace_id, id)
                .await
        }
    }
    .map_err(|_| Failed("unavailable"))?
    .filter(|stored| stored.version == secret.version)
    .ok_or(Failed("configuration_not_ready"))?;
    let workspace_id = matches!(installed, InstalledApp::Workspace(_)).then_some(session.workspace_id);
    let context = EncryptionContext::oauth_client_secret(
        session.organization_id,
        workspace_id,
        session.environment,
        app.id,
        stored.version,
    );
    let client_secret = keys::open(state.keys.as_ref(), &context, &stored.sealed)
        .await
        .map_err(|error| {
            tracing::error!(event = "connect.client_secret_unopenable", app_id = %app.id, %error);
            Failed("unavailable")
        })?;
    let client_secret = std::str::from_utf8(client_secret.expose()).map_err(|_| Failed("unavailable"))?;
    Ok((client_id.clone(), Zeroizing::new(client_secret.to_owned())))
}

/// `POST token_url` through the safe outbound path. A client with a secret
/// authenticates with HTTP Basic: the secret travels as the request's
/// credential, so it is scrubbed from the answer.
async fn exchange(
    state: &AppState,
    app: &App,
    token_url: &str,
    client: &TokenClient<'_>,
    code: &str,
    verifier: Option<&str>,
) -> Result<Tokens, Failed> {
    let redirect = redirect_uri(&state.public_url);
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect.as_str()),
    ];
    if let Some(verifier) = verifier {
        form.push(("code_verifier", verifier));
    }
    request_tokens(state, app.id, token_url, client, form).await
}

/// Shared token exchange for a code or a claimed refresh. Never retried.
pub(crate) async fn request_tokens<'a>(
    state: &AppState,
    app_id: zupflow_core::ids::AppId,
    token_url: &str,
    client: &TokenClient<'a>,
    mut form: Vec<(&'a str, &'a str)>,
) -> Result<Tokens, Failed> {
    // RFC 6749, section 2.3.1: each part form-encoded before joining.
    let basic = client.secret.map(|secret| {
        Zeroizing::new(format!(
            "{}:{}",
            url::form_urlencoded::byte_serialize(client.client_id.as_bytes()).collect::<String>(),
            Zeroizing::new(url::form_urlencoded::byte_serialize(secret.as_bytes()).collect::<String>()).as_str()
        ))
    });
    if client.secret.is_none() {
        // RFC 6749, section 4.1.3: a public client names itself.
        form.push(("client_id", client.client_id));
    }
    if let Some(resource) = client.resource {
        form.push(("resource", resource));
    }
    let body = Zeroizing::new(serde_urlencoded::to_string(&form).map_err(|_| Failed("unavailable"))?);
    let request = OutboundRequest {
        method: Method::Post,
        url: token_url.to_owned(),
        headers: vec![
            (
                "Content-Type".to_owned(),
                "application/x-www-form-urlencoded".to_owned(),
            ),
            ("Accept".to_owned(), "application/json".to_owned()),
        ],
        body: Some(body.as_bytes().to_vec()),
        credential: basic.map(|basic| Credential {
            header: "Authorization".to_owned(),
            value: Zeroizing::new(format!("Basic {}", STANDARD.encode(basic.as_bytes()))),
        }),
    };
    let response = match state.outbound.send(request, Limits::default()).await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(event = "connect.token_exchange_failed", %app_id, %error);
            return Err(Failed("provider_unavailable"));
        }
    };
    let body = Zeroizing::new(response.body);
    if !(200..300).contains(&response.status) {
        tracing::warn!(event = "connect.token_exchange_refused", %app_id, status = response.status);
        if matches!(response.status, 400 | 401)
            && serde_json::from_slice::<Value>(&body)
                .ok()
                .is_some_and(|v| v["error"] == "invalid_client")
        {
            return Err(Failed("invalid_client"));
        }
        return Err(Failed(if response.status >= 500 {
            "provider_unavailable"
        } else {
            "token_exchange_failed"
        }));
    }
    // The safe path scrubs the Basic credential it sent, not the secret
    // inside it: an answer that carries the secret back is refused whole.
    if let Some(secret) = client.secret {
        let encoded = Zeroizing::new(url::form_urlencoded::byte_serialize(secret.as_bytes()).collect::<String>());
        let escaped = Zeroizing::new(secret.replace('/', "\\/"));
        if [secret, encoded.as_str(), escaped.as_str()]
            .iter()
            .any(|secret| contains(&body, secret.as_bytes()))
        {
            tracing::warn!(event = "connect.token_response_echoes_secret", %app_id);
            return Err(Failed("token_exchange_failed"));
        }
    }
    if let Some(secret) = client.secret {
        fn contains_decoded(value: &Value, secret: &str) -> bool {
            match value {
                Value::String(text) => text.contains(secret),
                Value::Array(items) => items.iter().any(|value| contains_decoded(value, secret)),
                Value::Object(items) => items
                    .iter()
                    .any(|(key, value)| key.contains(secret) || contains_decoded(value, secret)),
                _ => false,
            }
        }
        if !secret.is_empty()
            && serde_json::from_slice::<Value>(&body)
                .ok()
                .is_some_and(|value| contains_decoded(&value, secret))
        {
            return Err(Failed("token_exchange_failed"));
        }
    }
    let tokens: TokenResponse = serde_json::from_slice(&body).map_err(|_| {
        tracing::warn!(event = "connect.token_response_unreadable", %app_id);
        Failed("token_exchange_failed")
    })?;
    if tokens.access_token.is_empty()
        || tokens.expires_in.is_some_and(|seconds| {
            seconds < 0
                || seconds
                    .checked_mul(1000)
                    .and_then(|millis| Timestamp::now().millis().checked_add(millis))
                    .is_none()
        })
        || tokens
            .token_type
            .as_deref()
            .is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer"))
    {
        tracing::warn!(event = "connect.token_response_unusable", %app_id);
        return Err(Failed("token_exchange_failed"));
    }
    Ok(Tokens {
        access_token: Zeroizing::new(tokens.access_token),
        refresh_token: tokens.refresh_token.map(Zeroizing::new),
        token_type: tokens.token_type,
        expires_in: tokens.expires_in,
        scope: tokens.scope,
    })
}

/// A string or a number, as identity endpoints name accounts.
fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value) if !value.trim().is_empty() => Some(value.trim().to_owned()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

/// `GET identity_url` with the new access token: the account is its `sub`
/// or `id`, named by its `name`, `login` or `email`.
async fn identify(state: &AppState, app: &App, url: &str, access_token: &str) -> Result<ConnectedAccount, Failed> {
    let request = OutboundRequest {
        method: Method::Get,
        url: url.to_owned(),
        headers: vec![("Accept".to_owned(), "application/json".to_owned())],
        body: None,
        credential: Some(Credential {
            header: "Authorization".to_owned(),
            value: Zeroizing::new(format!("Bearer {access_token}")),
        }),
    };
    let response = match state.outbound.send(request, Limits::default()).await {
        Ok(response) if (200..300).contains(&response.status) => response,
        Ok(response) => {
            tracing::warn!(event = "connect.identity_refused", app_id = %app.id, status = response.status);
            return Err(Failed("identity_unavailable"));
        }
        Err(error) => {
            tracing::warn!(event = "connect.identity_failed", app_id = %app.id, %error);
            return Err(Failed("identity_unavailable"));
        }
    };
    let identity: serde_json::Map<String, Value> = serde_json::from_slice(&response.body).map_err(|_| {
        tracing::warn!(event = "connect.identity_unreadable", app_id = %app.id);
        Failed("identity_unavailable")
    })?;
    let Some(external_id) = text(identity.get("sub")).or_else(|| text(identity.get("id"))) else {
        tracing::warn!(event = "connect.identity_without_id", app_id = %app.id);
        return Err(Failed("identity_unavailable"));
    };
    let email = text(identity.get("email")).filter(|email| email.contains('@'));
    let display_name = text(identity.get("name"))
        .or_else(|| text(identity.get("login")))
        .or_else(|| email.clone())
        .unwrap_or_else(|| format!("{} account", app.name));
    Ok(ConnectedAccount {
        external_id: external_id.chars().take(256).collect(),
        display_name: display_name.chars().take(200).collect(),
        email: email.map(|email| email.chars().take(320).collect()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_an_authorization_sealed_before_configured_issuers_existed() {
        let pending = serde_json::from_value::<Pending>(serde_json::json!({
            "reference": "old-reference", "verifier": "old-verifier", "client": null,
        }))
        .expect("a legacy sealed attempt stays readable");
        assert!(pending.issuer.is_none());
        assert!(pending.client.is_none());
    }

    #[test]
    fn sends_the_browser_only_where_a_configured_authorization_url_could_go() {
        for (url, allowed) in [
            ("https://auth.example/authorize", true),
            ("https://auth.example:8443/authorize?tenant=a", true),
            ("http://auth.example/authorize", false),
            ("https://localhost/authorize", false),
            ("https://auth.internal/authorize", false),
            ("https://10.0.0.7/authorize", false),
            ("https://169.254.169.254/authorize", false),
            ("https://[::1]/authorize", false),
            ("https://user:pw@auth.example/authorize", false),
            ("javascript:alert(1)", false),
            ("/authorize", false),
        ] {
            assert_eq!(public_https(url), allowed, "{url}");
        }
    }
}
