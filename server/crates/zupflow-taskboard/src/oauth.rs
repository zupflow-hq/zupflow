//! Taskboard's authorization server: metadata, client registration, the
//! authorization code flow with PKCE, tokens and the account.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::pages::{self, Problem};
use crate::{
    ACCOUNT_EMAIL, ACCOUNT_ID, ACCOUNT_NAME, AccessToken, CODE_TTL, CONSENT_TTL, Client, Code, Consent, RefreshToken,
    SCOPES, Taskboard, TokenRequest, later, now_ms, random,
};

const GRANT_TYPES: [&str; 2] = ["authorization_code", "refresh_token"];

/// An OAuth error answer (RFC 6749, section 5.2), never cached.
pub(crate) fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({ "error": error, "error_description": description })),
    )
        .into_response()
}

pub(crate) async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "not_found", "error_description": "Taskboard serves nothing here." })),
    )
        .into_response()
}

/// RFC 9728: the MCP endpoint, and this origin as its authorization server.
pub(crate) async fn protected_resource(State(board): State<Taskboard>) -> Json<Value> {
    Json(json!({
        "resource": board.mcp_url(),
        "resource_name": "Taskboard",
        "authorization_servers": [board.base_url()],
        "scopes_supported": SCOPES.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        "bearer_methods_supported": ["header"],
    }))
}

/// RFC 8414.
pub(crate) async fn authorization_server(State(board): State<Taskboard>) -> Json<Value> {
    let base = board.base_url();
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "registration_endpoint": format!("{base}/register"),
        "scopes_supported": SCOPES.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": GRANT_TYPES,
        "token_endpoint_auth_methods_supported": ["none"],
        "code_challenge_methods_supported": ["S256"],
        "authorization_response_iss_parameter_supported": true,
    }))
}

fn content_type_is(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(expected))
}

/// A redirect URI a client may register: HTTPS, or HTTP on loopback (RFC
/// 8252), absolute, without a fragment or credentials.
fn usable_redirect_uri(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    (url.scheme() == "https" || (url.scheme() == "http" && loopback))
        && url.host_str().is_some()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && value.len() <= 2048
}

/// A list of strings, each one of `allowed`; `None` if absent.
fn subset(value: Option<&Value>, allowed: &[&str]) -> Result<Option<Vec<String>>, ()> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) if !items.is_empty() => items
            .iter()
            .map(|item| match item.as_str() {
                Some(item) if allowed.contains(&item) => Ok(item.to_owned()),
                _ => Err(()),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(()),
    }
}

/// RFC 7591: registers a public client.
pub(crate) async fn register(State(board): State<Taskboard>, headers: HeaderMap, body: Bytes) -> Response {
    let invalid = |description: &str| oauth_error(StatusCode::BAD_REQUEST, "invalid_client_metadata", description);
    if !content_type_is(&headers, "application/json") {
        return invalid("Send the client's metadata as application/json.");
    }
    let Ok(Value::Object(metadata)) = serde_json::from_slice::<Value>(&body) else {
        return invalid("The client's metadata must be a JSON object.");
    };
    let redirect_uris: Vec<String> = match metadata.get("redirect_uris") {
        Some(Value::Array(uris)) if !uris.is_empty() && uris.len() <= 10 => {
            let uris: Option<Vec<String>> = uris
                .iter()
                .map(|uri| uri.as_str().filter(|uri| usable_redirect_uri(uri)).map(str::to_owned))
                .collect();
            match uris {
                Some(uris) => uris,
                None => {
                    return oauth_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_redirect_uri",
                        "Each redirect URI must be an absolute HTTPS URL, or HTTP on loopback, without a fragment.",
                    );
                }
            }
        }
        _ => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_redirect_uri",
                "Name between 1 and 10 redirect URIs.",
            );
        }
    };
    match metadata.get("token_endpoint_auth_method") {
        None | Some(Value::Null) => {}
        Some(Value::String(method)) if method == "none" => {}
        Some(_) => return invalid("Taskboard registers public clients only: token_endpoint_auth_method none."),
    }
    let Ok(grant_types) = subset(metadata.get("grant_types"), &GRANT_TYPES) else {
        return invalid("grant_types may name authorization_code and refresh_token only.");
    };
    let Ok(response_types) = subset(metadata.get("response_types"), &["code"]) else {
        return invalid("response_types may name code only.");
    };
    let client_name = match metadata.get("client_name") {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) if !name.trim().is_empty() && name.chars().count() <= 100 => {
            Some(name.trim().to_owned())
        }
        Some(_) => return invalid("client_name must be 1 to 100 characters."),
    };
    let client_id = random("tbc_");
    {
        let mut state = board.state();
        state.clients.insert(
            client_id.clone(),
            Client {
                name: client_name.clone(),
                redirect_uris: redirect_uris.clone(),
            },
        );
        state.client_order.push(client_id.clone());
    }
    (
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "client_id": client_id,
            "client_id_issued_at": now_ms() / 1000,
            "client_name": client_name,
            "redirect_uris": redirect_uris,
            "grant_types": grant_types.unwrap_or_else(|| vec!["authorization_code".to_owned()]),
            "response_types": response_types.unwrap_or_else(|| vec!["code".to_owned()]),
            "token_endpoint_auth_method": "none",
        })),
    )
        .into_response()
}

/// Query or form pairs, refusing a parameter given twice (RFC 6749, section 3.1).
fn pairs(raw: &[u8]) -> Result<Vec<(String, String)>, ()> {
    let pairs: Vec<(String, String)> = url::form_urlencoded::parse(raw).into_owned().collect();
    for (index, (name, _)) in pairs.iter().enumerate() {
        if pairs[..index].iter().any(|(earlier, _)| earlier == name) {
            return Err(());
        }
    }
    Ok(pairs)
}

fn param<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
        .filter(|value| !value.is_empty())
}

/// Sends the browser back to the client with `params` and the issuer (RFC 9207).
fn back_to_client(board: &Taskboard, redirect_uri: &str, params: &[(&str, &str)]) -> Response {
    let Ok(mut url) = url::Url::parse(redirect_uri) else {
        return pages::problem(Problem::BadRedirect);
    };
    {
        let mut query = url.query_pairs_mut();
        for (name, value) in params {
            query.append_pair(name, value);
        }
        query.append_pair("iss", board.base_url());
    }
    let mut response = StatusCode::SEE_OTHER.into_response();
    if let Ok(location) = HeaderValue::try_from(url.as_str()) {
        response.headers_mut().insert(header::LOCATION, location);
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// A PKCE `S256` challenge: 43 base64url characters.
fn plausible_challenge(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// The start of the flow: checks the request, then asks the person.
pub(crate) async fn authorize(State(board): State<Taskboard>, RawQuery(query): RawQuery) -> Response {
    let raw = query.unwrap_or_default();
    let all = pairs(raw.as_bytes());
    let lenient: Vec<(String, String)> = url::form_urlencoded::parse(raw.as_bytes()).into_owned().collect();
    // Who to answer, before anything else: an unknown client or a redirect
    // URI it didn't register is never redirected to (RFC 6749, 4.1.2.1).
    let Some(client_id) = param(&lenient, "client_id").map(str::to_owned) else {
        return pages::problem(Problem::UnknownClient);
    };
    let (client_name, registered) = {
        let state = board.state();
        match state.clients.get(&client_id) {
            Some(client) => (client.name.clone(), client.redirect_uris.clone()),
            None => return pages::problem(Problem::UnknownClient),
        }
    };
    let Some(redirect_uri) = param(&lenient, "redirect_uri").map(str::to_owned) else {
        return pages::problem(Problem::BadRedirect);
    };
    if !registered.contains(&redirect_uri) {
        return pages::problem(Problem::BadRedirect);
    }
    let state_param = param(&lenient, "state").unwrap_or_default().to_owned();
    let fail = |error: &str, description: &str| {
        let mut params = vec![("error", error), ("error_description", description)];
        if !state_param.is_empty() {
            params.push(("state", state_param.as_str()));
        }
        back_to_client(&board, &redirect_uri, &params)
    };
    let Ok(all) = all else {
        return fail("invalid_request", "A parameter was given more than once.");
    };
    if param(&all, "response_type") != Some("code") {
        return fail("unsupported_response_type", "Only response_type=code is supported.");
    }
    if state_param.is_empty() || state_param.len() > 512 {
        return fail("invalid_request", "Taskboard requires a state of up to 512 characters.");
    }
    if param(&all, "code_challenge_method") != Some("S256") {
        return fail("invalid_request", "PKCE is required, with code_challenge_method=S256.");
    }
    let Some(code_challenge) = param(&all, "code_challenge").filter(|value| plausible_challenge(value)) else {
        return fail("invalid_request", "code_challenge must be 43 base64url characters.");
    };
    match param(&all, "resource") {
        None => return fail("invalid_request", "Name the MCP server as the resource (RFC 8707)."),
        Some(resource) if resource != board.mcp_url() => {
            return fail("invalid_target", "Taskboard issues tokens for its MCP server only.");
        }
        Some(_) => {}
    }
    let scopes: Vec<String> = match param(&all, "scope") {
        None => SCOPES.iter().map(|(name, _)| (*name).to_owned()).collect(),
        Some(scope) => {
            let mut scopes: Vec<String> = Vec::new();
            for name in scope.split(' ') {
                if !SCOPES.iter().any(|(known, _)| *known == name) {
                    return fail("invalid_scope", "Taskboard knows tasks:read and tasks:write only.");
                }
                if !scopes.iter().any(|kept| kept == name) {
                    scopes.push(name.to_owned());
                }
            }
            scopes
        }
    };
    let consent = Consent {
        client_id,
        redirect_uri,
        state: state_param.clone(),
        code_challenge: code_challenge.to_owned(),
        scopes,
        expires_at: later(CONSENT_TTL),
    };
    let id = random("tbr_");
    board.state().consents.insert(id.clone(), consent.clone());
    pages::consent(&id, client_name.as_deref(), &consent.scopes)
}

/// Whether a browser request came from Taskboard's own page: no `Origin`
/// (a request no page made), or exactly this origin.
pub(crate) fn same_origin(board: &Taskboard, headers: &HeaderMap) -> bool {
    match headers.get(header::ORIGIN) {
        None => true,
        Some(origin) => origin.as_bytes() == board.base_url().as_bytes(),
    }
}

/// The consent page's answer: a code, or `access_denied`.
pub(crate) async fn decide(State(board): State<Taskboard>, headers: HeaderMap, body: Bytes) -> Response {
    if !same_origin(&board, &headers) {
        return pages::problem(Problem::ForeignOrigin);
    }
    let Ok(form) = pairs(&body) else {
        return pages::problem(Problem::BadDecision);
    };
    let Some(id) = param(&form, "request") else {
        return pages::problem(Problem::BadDecision);
    };
    let approved = match param(&form, "decision") {
        Some("approve") => true,
        Some("deny") => false,
        _ => return pages::problem(Problem::BadDecision),
    };
    // Single use: taken out whatever the answer.
    let Some(consent) = board.state().consents.remove(id) else {
        return pages::problem(Problem::UnknownRequest);
    };
    if now_ms() >= consent.expires_at {
        return pages::problem(Problem::UnknownRequest);
    }
    if !approved {
        return back_to_client(
            &board,
            &consent.redirect_uri,
            &[
                ("error", "access_denied"),
                ("error_description", "The person denied access."),
                ("state", &consent.state),
            ],
        );
    }
    let code = random("tbx_");
    board.state().codes.insert(
        code.clone(),
        Code {
            client_id: consent.client_id,
            redirect_uri: consent.redirect_uri.clone(),
            code_challenge: consent.code_challenge,
            scopes: consent.scopes,
            expires_at: later(CODE_TTL),
            used: false,
            family: random("tbf_"),
        },
    );
    back_to_client(
        &board,
        &consent.redirect_uri,
        &[("code", &code), ("state", &consent.state)],
    )
}

/// Issues an access token and a refresh token in `family`, and answers them.
fn issue(board: &Taskboard, client_id: &str, scopes: &[String], family: &str) -> Response {
    let access = random("tbat_");
    let refresh = random("tbrt_");
    let ttl = board.inner.access_token_ttl;
    {
        let mut state = board.state();
        state.access_tokens.insert(
            access.clone(),
            AccessToken {
                client_id: client_id.to_owned(),
                scopes: scopes.to_vec(),
                expires_at: later(ttl),
                family: family.to_owned(),
                revoked: false,
            },
        );
        state
            .issued
            .entry(client_id.to_owned())
            .or_default()
            .push(access.clone());
        state.refresh_tokens.insert(
            refresh.clone(),
            RefreshToken {
                client_id: client_id.to_owned(),
                scopes: scopes.to_vec(),
                family: family.to_owned(),
                used: false,
                revoked: false,
            },
        );
    }
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store"), (header::PRAGMA, "no-cache")],
        Json(json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": ttl.as_secs(),
            "refresh_token": refresh,
            "scope": scopes.join(" "),
        })),
    )
        .into_response()
}

/// The token endpoint: a code with its verifier, or a refresh token.
pub(crate) async fn token(State(board): State<Taskboard>, headers: HeaderMap, body: Bytes) -> Response {
    let form = pairs(&body);
    let lenient: Vec<(String, String)> = url::form_urlencoded::parse(&body).into_owned().collect();
    let grant_type = param(&lenient, "grant_type").unwrap_or_default().to_owned();
    let client_id = param(&lenient, "client_id").map(str::to_owned);
    let resource = param(&lenient, "resource").map(str::to_owned);
    let answer = exchange(&board, &headers, form);
    board.state().token_requests.push(TokenRequest {
        grant_type,
        client_id,
        resource,
        error: answer.error,
    });
    answer.response
}

/// A token endpoint answer, with its error code kept for the record.
struct Answer {
    response: Response,
    error: Option<String>,
}

impl Answer {
    fn error(status: StatusCode, error: &str, description: &str) -> Self {
        Self {
            response: oauth_error(status, error, description),
            error: Some(error.to_owned()),
        }
    }

    fn ok(response: Response) -> Self {
        Self { response, error: None }
    }
}

fn exchange(board: &Taskboard, headers: &HeaderMap, form: Result<Vec<(String, String)>, ()>) -> Answer {
    let bad = |error: &str, description: &str| Answer::error(StatusCode::BAD_REQUEST, error, description);
    if !content_type_is(headers, "application/x-www-form-urlencoded") {
        return bad(
            "invalid_request",
            "Send the request as application/x-www-form-urlencoded.",
        );
    }
    let Ok(form) = form else {
        return bad("invalid_request", "A parameter was given more than once.");
    };
    if headers.contains_key(header::AUTHORIZATION) {
        return Answer::error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "Taskboard's clients are public: they name themselves with client_id and send no secret.",
        );
    }
    let Some(client_id) = param(&form, "client_id") else {
        return Answer::error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "Name the client with client_id.",
        );
    };
    if !board.state().clients.contains_key(client_id) {
        return Answer::error(StatusCode::UNAUTHORIZED, "invalid_client", "No client has that ID.");
    }
    // RFC 8707: every grant names the MCP server, as a strict server may require.
    if param(&form, "resource") != Some(board.mcp_url().as_str()) {
        return bad(
            "invalid_target",
            "Name Taskboard's MCP server as the resource (RFC 8707).",
        );
    }
    match param(&form, "grant_type") {
        Some("authorization_code") => exchange_code(board, &form, client_id),
        Some("refresh_token") => refresh(board, &form, client_id),
        Some(_) => bad("unsupported_grant_type", "Use authorization_code or refresh_token."),
        None => bad("invalid_request", "Name the grant_type."),
    }
}

fn exchange_code(board: &Taskboard, form: &[(String, String)], client_id: &str) -> Answer {
    let bad = |error: &str, description: &str| Answer::error(StatusCode::BAD_REQUEST, error, description);
    let (Some(code), Some(redirect_uri), Some(verifier)) = (
        param(form, "code"),
        param(form, "redirect_uri"),
        param(form, "code_verifier"),
    ) else {
        return bad("invalid_request", "Send code, redirect_uri and code_verifier.");
    };
    let mut state = board.state();
    let Some(grant) = state.codes.get_mut(code) else {
        return bad("invalid_grant", "That code isn't one Taskboard issued.");
    };
    if grant.used {
        // RFC 6749, 4.1.2: a code used twice revokes what it was exchanged for.
        let family = grant.family.clone();
        state.revoke_family(&family);
        return bad(
            "invalid_grant",
            "That code was already used; the tokens it gave are revoked.",
        );
    }
    if grant.client_id != client_id {
        return bad("invalid_grant", "That code was issued to another client.");
    }
    if now_ms() >= grant.expires_at {
        return bad("invalid_grant", "That code expired. Start again.");
    }
    if grant.redirect_uri != redirect_uri {
        return bad("invalid_grant", "redirect_uri isn't the one the code was issued for.");
    }
    let verified = (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._~".contains(&byte))
        && URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())) == grant.code_challenge;
    if !verified {
        // A failed attempt still spends the code.
        grant.used = true;
        return bad("invalid_grant", "code_verifier doesn't match the code_challenge.");
    }
    grant.used = true;
    let scopes = grant.scopes.clone();
    let family = grant.family.clone();
    drop(state);
    Answer::ok(issue(board, client_id, &scopes, &family))
}

fn refresh(board: &Taskboard, form: &[(String, String)], client_id: &str) -> Answer {
    let bad = |error: &str, description: &str| Answer::error(StatusCode::BAD_REQUEST, error, description);
    let Some(presented) = param(form, "refresh_token") else {
        return bad("invalid_request", "Send the refresh_token.");
    };
    let mut state = board.state();
    let Some(token) = state.refresh_tokens.get_mut(presented) else {
        return bad("invalid_grant", "That refresh token isn't one Taskboard issued.");
    };
    if token.client_id != client_id {
        return bad("invalid_grant", "That refresh token was issued to another client.");
    }
    if token.revoked {
        return bad("invalid_grant", "That refresh token was revoked. Sign in again.");
    }
    if token.used {
        // Rotation: a refresh token used again means it leaked; revoke them all.
        let family = token.family.clone();
        state.revoke_family(&family);
        return bad(
            "invalid_grant",
            "That refresh token was already used; its tokens are revoked.",
        );
    }
    let mut scopes = token.scopes.clone();
    if let Some(scope) = param(form, "scope") {
        let narrowed: Vec<String> = scope.split(' ').map(str::to_owned).collect();
        if narrowed.iter().any(|name| !scopes.contains(name)) {
            return bad("invalid_scope", "A refresh may narrow the scope, never widen it.");
        }
        scopes = narrowed;
    }
    token.used = true;
    let family = token.family.clone();
    drop(state);
    Answer::ok(issue(board, client_id, &scopes, &family))
}

/// Who a valid access token acts for: its client, scopes and the token itself.
pub(crate) struct Bearer {
    pub client_id: String,
    pub scopes: Vec<String>,
    pub token: String,
}

/// `401` with what a client needs to sign in (RFC 6750, RFC 9728).
pub(crate) fn unauthorized(board: &Taskboard, error: Option<(&str, &str)>) -> Response {
    let metadata = board.resource_metadata_url();
    let challenge = match error {
        None => format!(r#"Bearer resource_metadata="{metadata}""#),
        Some((error, description)) => {
            format!(r#"Bearer error="{error}", error_description="{description}", resource_metadata="{metadata}""#)
        }
    };
    let mut response = (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": error.map_or("unauthorized", |(error, _)| error) })),
    )
        .into_response();
    if let Ok(value) = HeaderValue::try_from(challenge) {
        response.headers_mut().insert(header::WWW_AUTHENTICATE, value);
    }
    response
}

/// Why a request's bearer token was refused: an OAuth error and its
/// description, or nothing for a request without one.
pub(crate) struct Refused(Option<(&'static str, &'static str)>);

impl Refused {
    pub(crate) fn answer(self, board: &Taskboard) -> Response {
        unauthorized(board, self.0)
    }
}

/// The bearer token on a request, checked: issued here, live, not revoked.
pub(crate) fn bearer(board: &Taskboard, headers: &HeaderMap) -> Result<Bearer, Refused> {
    let Some(value) = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok()) else {
        return Err(Refused(None));
    };
    let Some(token) = value
        .split_once(' ')
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim())
    else {
        return Err(Refused(Some((
            "invalid_request",
            "Send the token as Authorization: Bearer.",
        ))));
    };
    let state = board.state();
    let invalid = |description| Err(Refused(Some(("invalid_token", description))));
    match state.access_tokens.get(token) {
        None => invalid("The access token isn't one Taskboard issued."),
        Some(found) if found.revoked => invalid("The access token was revoked."),
        Some(found) if now_ms() >= found.expires_at => invalid("The access token expired."),
        Some(found) => Ok(Bearer {
            client_id: found.client_id.clone(),
            scopes: found.scopes.clone(),
            token: token.to_owned(),
        }),
    }
}

/// The account a token acts for.
pub(crate) async fn me(State(board): State<Taskboard>, headers: HeaderMap) -> Response {
    match bearer(&board, &headers) {
        Ok(_) => Json(json!({
            "sub": ACCOUNT_ID,
            "name": ACCOUNT_NAME,
            "email": ACCOUNT_EMAIL,
        }))
        .into_response(),
        Err(refused) => refused.answer(&board),
    }
}
