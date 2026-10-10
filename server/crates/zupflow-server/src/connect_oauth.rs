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
use zupflow_core::apps::{App, AuthTemplate, Configuration, OAuthScope};
use zupflow_core::connect::{
    Authorization, Completion, ConnectError, ConnectIntent, ConnectSession, ConnectSessionStatus, OAuthState,
    Reference, ReferenceHash,
};
use zupflow_core::error::ApiError;
use zupflow_core::ids::InstallationId;
use zupflow_core::installations::{ConnectedAccount, InstalledApp, NewCredential, TemplateSnapshot};
use zupflow_core::keys::{self, EncryptionContext};
use zupflow_core::outbound::{Credential, Limits, Method, OutboundRequest};
use zupflow_core::time::Timestamp;

use crate::app_configurations::redirect_uri;
use crate::auth::AppState;
use crate::connect::{attributed, connect_failure, not_pending, still_eligible};
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
}

/// [`Sealing`], opened.
#[derive(Deserialize)]
struct Pending {
    reference: String,
    verifier: Option<String>,
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

fn authorization_context(session: &ConnectSession, attempt: u32) -> EncryptionContext {
    EncryptionContext::connect_authorization(
        session.organization_id,
        session.workspace_id,
        session.environment,
        session.app.id(),
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
    let (app, configuration) = still_eligible(&state, &session).await?;
    let AuthTemplate::OAuth2 {
        authorize_url,
        token_url,
        pkce,
        ..
    } = &app.auth
    else {
        return Err(ApiError::new(422, "wrong_auth_type", format!("{} connects with a token.", app.name)).into());
    };
    let (Some(authorize_url), Some(_)) = (authorize_url, token_url) else {
        return Err(endpoints_missing(&app));
    };
    let Some(client_id) = &configuration.oauth_client_id else {
        // A ready OAuth configuration has one.
        tracing::error!(event = "connect.oauth_without_client_id", app_id = %app.id);
        return Err(ApiError::internal().into());
    };
    let Some(attempt) = session.authorization_attempt.checked_add(1) else {
        return Err(ApiError::internal().into());
    };

    let oauth_state = OAuthState::generate().map_err(connect_failure)?;
    let verifier = if *pkce { Some(verifier()?) } else { None };
    let sealing = Sealing {
        reference: &reference,
        verifier: verifier.as_deref().map(String::as_str),
    };
    let plaintext = Zeroizing::new(serde_json::to_vec(&sealing).map_err(|_| Failure(ApiError::internal()))?);
    let sealed = keys::seal(
        state.keys.as_ref(),
        &authorization_context(&session, attempt),
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

    // Checked when the app saved its template (decision 0035).
    let mut url = url::Url::parse(authorize_url).map_err(|_| {
        tracing::error!(event = "connect.authorize_url_unparseable", app_id = %app.id);
        Failure(ApiError::internal())
    })?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
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
    }
    Ok(envelope(
        StatusCode::OK,
        AuthorizeView {
            authorization_url: url.into(),
        },
    ))
}

// --- The callback -----------------------------------------------------------

/// A callback that names no attempt we hold: no `state`, an unknown one, or
/// one already used. Nothing of any session is said.
fn unknown_state() -> Response {
    Failure(ApiError::new(
        400,
        "invalid_oauth_state",
        "This sign-in link isn't valid or was already used. Start again from the app.",
    ))
    .into_response()
}

/// Back to the hosted page, which reads the session's outcome. Neither the
/// callback's URL (it carries the code) nor the page's (it carries the
/// reference) is sent on as a referrer.
fn back_to(state: &AppState, reference: &str) -> Response {
    let mut response = Redirect::to(&format!("{}/connect/{reference}", state.public_url)).into_response();
    let headers = response.headers_mut();
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Why a callback didn't complete its session: the session's `failure_code`.
struct Failed(&'static str);

/// The provider's answer: the browser returns with `code`, or with `error`.
/// The code is used once, at the token endpoint, and never logged.
pub async fn callback(State(state): State<AppState>, RawQuery(query): RawQuery) -> Response {
    let params: BTreeMap<String, String> = query
        .as_deref()
        .and_then(|query| serde_urlencoded::from_str(query).ok())
        .unwrap_or_default();
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
    let opened = keys::open(
        state.keys.as_ref(),
        &authorization_context(&session, authorization.attempt),
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

    let outcome = match (params.get("code"), params.get("error")) {
        (_, Some(error)) => {
            // The provider's own words are never passed on, only whether consent was refused.
            tracing::info!(event = "connect.oauth_error", session_id = %session.id, denied = error == "access_denied");
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
            )
            .await
        }
        _ => Err(Failed("authorization_failed")),
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
    token_type: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
}

/// What an installation's OAuth credential holds, sealed as one value.
#[derive(Serialize)]
struct OAuthCredential<'a> {
    access_token: &'a str,
    refresh_token: Option<&'a str>,
    token_type: Option<&'a str>,
}

/// Exchanges the code, identifies the account, and completes the session
/// with the sealed tokens, revalidating everything the start relied on.
async fn complete(
    state: &AppState,
    session: &ConnectSession,
    code: &str,
    verifier: Option<&str>,
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
    let AuthTemplate::OAuth2 {
        token_url: Some(token_url),
        identity_url,
        ..
    } = &app.auth
    else {
        // The template changed since the start.
        return Err(Failed("configuration_not_ready"));
    };
    let tokens = exchange(state, session, &app, &configuration, token_url, code, verifier).await?;
    let identified = match identity_url {
        Some(url) => Some(identify(state, &app, url, &tokens.access_token).await?),
        None => None,
    };

    let now = Timestamp::now();
    let (installation_id, account, version) = match session.intent {
        ConnectIntent::Create => {
            let installation_id = InstallationId::generate();
            let account = identified.unwrap_or_else(|| ConnectedAccount {
                external_id: installation_id.to_string(),
                display_name: format!("{} account", app.name),
                email: None,
            });
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
            match (installation.connection, identified) {
                // A reconnect must use the same account (decision 0005).
                (Some(connection), Some(account)) if account.external_id != connection.account.external_id => {
                    return Err(Failed("account_mismatch"));
                }
                (Some(connection), identified) => (
                    installation_id,
                    identified.unwrap_or(connection.account),
                    connection.credential_version + 1,
                ),
                (None, identified) => (
                    installation_id,
                    identified.unwrap_or_else(|| ConnectedAccount {
                        external_id: installation_id.to_string(),
                        display_name: format!("{} account", app.name),
                        email: None,
                    }),
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
        .filter(|seconds| *seconds > 0)
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
        )
        .await
    {
        Ok(_) | Err(ConnectError::NotPending) => Ok(()),
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
struct Tokens {
    access_token: Zeroizing<String>,
    refresh_token: Option<Zeroizing<String>>,
    token_type: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
}

/// `POST token_url` through the safe outbound path, the client
/// authenticating with HTTP Basic (`client_secret_basic`): the secret
/// travels as the request's credential, so it is scrubbed from the answer.
async fn exchange(
    state: &AppState,
    session: &ConnectSession,
    app: &App,
    configuration: &Configuration,
    token_url: &str,
    code: &str,
    verifier: Option<&str>,
) -> Result<Tokens, Failed> {
    let (Some(client_id), Some(secret)) = (&configuration.oauth_client_id, &configuration.client_secret) else {
        return Err(Failed("configuration_not_ready"));
    };
    let stored = match session.app {
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
    let workspace_id = matches!(session.app, InstalledApp::Workspace(_)).then_some(session.workspace_id);
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
    // RFC 6749, section 2.3.1: each part form-encoded before joining.
    let basic = Zeroizing::new(format!(
        "{}:{}",
        url::form_urlencoded::byte_serialize(client_id.as_bytes()).collect::<String>(),
        Zeroizing::new(url::form_urlencoded::byte_serialize(client_secret.as_bytes()).collect::<String>()).as_str()
    ));
    let redirect = redirect_uri(&state.public_url);
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect.as_str()),
    ];
    if let Some(verifier) = verifier {
        form.push(("code_verifier", verifier));
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
        credential: Some(Credential {
            header: "Authorization".to_owned(),
            value: Zeroizing::new(format!("Basic {}", STANDARD.encode(basic.as_bytes()))),
        }),
    };
    let response = match state.outbound.send(request, Limits::default()).await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(event = "connect.token_exchange_failed", app_id = %app.id, %error);
            return Err(Failed("provider_unavailable"));
        }
    };
    let body = Zeroizing::new(response.body);
    if !(200..300).contains(&response.status) {
        tracing::warn!(event = "connect.token_exchange_refused", app_id = %app.id, status = response.status);
        return Err(Failed(if response.status >= 500 {
            "provider_unavailable"
        } else {
            "token_exchange_failed"
        }));
    }
    // The safe path scrubs the Basic credential it sent, not the secret
    // inside it: an answer that carries the secret back is refused whole.
    let encoded = Zeroizing::new(url::form_urlencoded::byte_serialize(client_secret.as_bytes()).collect::<String>());
    let escaped = Zeroizing::new(client_secret.replace('/', "\\/"));
    if [client_secret, encoded.as_str(), escaped.as_str()]
        .iter()
        .any(|secret| contains(&body, secret.as_bytes()))
    {
        tracing::warn!(event = "connect.token_response_echoes_secret", app_id = %app.id);
        return Err(Failed("token_exchange_failed"));
    }
    let tokens: TokenResponse = serde_json::from_slice(&body).map_err(|_| {
        tracing::warn!(event = "connect.token_response_unreadable", app_id = %app.id);
        Failed("token_exchange_failed")
    })?;
    if tokens.access_token.is_empty()
        || tokens
            .token_type
            .as_deref()
            .is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer"))
    {
        tracing::warn!(event = "connect.token_response_unusable", app_id = %app.id);
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
