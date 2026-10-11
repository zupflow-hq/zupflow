//! Taskboard is strict where a real MCP server with OAuth must be: these
//! are the rules Zupflow's end-to-end test relies on it to enforce.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::net::SocketAddr;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use zupflow_taskboard::{Options, Running, start};

const REDIRECT: &str = "http://localhost:8080/connect/oauth/callback";
const VERIFIER: &str = "verifier-0123456789-0123456789-0123456789-0123456789";

struct Fixture {
    running: Running,
    http: Client,
}

impl Fixture {
    async fn new() -> Self {
        let running = start(Options::default()).await.unwrap();
        let http = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .resolve("taskboard.example", SocketAddr::from(([127, 0, 0, 1], running.port)))
            .tls_certs_only([reqwest::Certificate::from_pem(running.certificate_pem.as_bytes()).unwrap()])
            .build()
            .unwrap();
        Self { running, http }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.running.taskboard.base_url())
    }

    async fn register(&self, body: Value) -> (StatusCode, Value) {
        let response = self.http.post(self.url("/register")).json(&body).send().await.unwrap();
        (response.status(), response.json().await.unwrap())
    }

    async fn client(&self) -> String {
        let (status, body) = self
            .register(
                json!({ "client_name": "Zupflow", "redirect_uris": [REDIRECT], "token_endpoint_auth_method": "none" }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["client_id"].as_str().unwrap().to_owned()
    }

    fn authorize_url(&self, params: &[(&str, &str)]) -> String {
        let mut url = url::Url::parse(&self.url("/authorize")).unwrap();
        url.query_pairs_mut().extend_pairs(params);
        url.into()
    }

    /// A correct authorization request's parameters for `client_id`.
    fn params(&self, client_id: &str) -> Vec<(&'static str, String)> {
        vec![
            ("response_type", "code".to_owned()),
            ("client_id", client_id.to_owned()),
            ("redirect_uri", REDIRECT.to_owned()),
            ("state", "s-1".to_owned()),
            ("scope", "tasks:read tasks:write".to_owned()),
            ("code_challenge", challenge(VERIFIER)),
            ("code_challenge_method", "S256".to_owned()),
            ("resource", self.running.taskboard.mcp_url()),
        ]
    }

    /// Visits the authorization URL: its status, `Location` and body.
    async fn visit(&self, params: &[(&str, String)]) -> (StatusCode, Option<String>, String) {
        let params: Vec<(&str, &str)> = params.iter().map(|(name, value)| (*name, value.as_str())).collect();
        let response = self.http.get(self.authorize_url(&params)).send().await.unwrap();
        let location = response
            .headers()
            .get("location")
            .map(|value| value.to_str().unwrap().to_owned());
        (response.status(), location, response.text().await.unwrap())
    }

    /// Answers the consent page: the redirect's query.
    async fn decide(&self, page: &str, decision: &str) -> BTreeMap<String, String> {
        let request = hidden(page, "request");
        let response = self
            .http
            .post(self.url("/authorize"))
            .form(&[("request", request.as_str()), ("decision", decision)])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        query(response.headers().get("location").unwrap().to_str().unwrap())
    }

    /// A code for `client_id`, approved.
    async fn code(&self, client_id: &str) -> String {
        let (status, _, page) = self.visit(&self.params(client_id)).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let answer = self.decide(&page, "approve").await;
        assert_eq!(answer["state"], "s-1");
        assert_eq!(answer["iss"], self.running.taskboard.base_url());
        answer["code"].clone()
    }

    async fn token(&self, form: &[(&str, &str)]) -> (StatusCode, Value) {
        let response = self.http.post(self.url("/token")).form(form).send().await.unwrap();
        assert_eq!(response.headers()["cache-control"], "no-store");
        (response.status(), response.json().await.unwrap())
    }

    async fn exchange(&self, client_id: &str, code: &str) -> (StatusCode, Value) {
        self.token(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", REDIRECT),
            ("client_id", client_id),
            ("code_verifier", VERIFIER),
            ("resource", &self.running.taskboard.mcp_url()),
        ])
        .await
    }

    async fn tokens(&self, client_id: &str) -> Value {
        let code = self.code(client_id).await;
        let (status, tokens) = self.exchange(client_id, &code).await;
        assert_eq!(status, StatusCode::OK, "{tokens}");
        tokens
    }

    /// One MCP message: status, headers and the JSON answer (or null).
    async fn mcp(&self, token: Option<&str>, session: Option<&str>, message: Value) -> (StatusCode, HeaderMap, Value) {
        let mut request = self
            .http
            .post(self.running.taskboard.mcp_url())
            .header("accept", "application/json, text/event-stream")
            .json(&message);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(session) = session {
            request = request
                .header("mcp-session-id", session)
                .header("mcp-protocol-version", "2025-06-18");
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let text = response.text().await.unwrap();
        (status, headers, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn session(&self, token: &str) -> String {
        let (status, headers, body) = self
            .mcp(
                Some(token),
                None,
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "1" } } }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["result"]["protocolVersion"], "2025-06-18");
        headers["mcp-session-id"].to_str().unwrap().to_owned()
    }

    async fn call(&self, token: &str, session: &str, tool: &str, arguments: Value) -> (StatusCode, Value) {
        let (status, _, body) = self
            .mcp(
                Some(token),
                Some(session),
                json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": tool, "arguments": arguments } }),
            )
            .await;
        (status, body)
    }
}

fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn hidden(page: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let start = page.find(&marker).unwrap() + marker.len();
    let end = start + page[start..].find('"').unwrap();
    page[start..end].to_owned()
}

fn query(location: &str) -> BTreeMap<String, String> {
    url::Url::parse(location).unwrap().query_pairs().into_owned().collect()
}

#[tokio::test]
async fn serves_its_metadata_and_a_challenge() {
    let f = Fixture::new().await;
    let base = f.running.taskboard.base_url().to_owned();
    let (status, headers, _) = f
        .mcp(None, None, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        headers["www-authenticate"],
        HeaderValue::from_str(&format!(
            r#"Bearer resource_metadata="{base}/.well-known/oauth-protected-resource/mcp""#
        ))
        .unwrap()
    );
    for path in [
        "/.well-known/oauth-protected-resource/mcp",
        "/.well-known/oauth-protected-resource",
    ] {
        let resource: Value = f.http.get(f.url(path)).send().await.unwrap().json().await.unwrap();
        assert_eq!(resource["resource"], format!("{base}/mcp"));
        assert_eq!(resource["authorization_servers"], json!([base]));
    }
    let metadata: Value = f
        .http
        .get(f.url("/.well-known/oauth-authorization-server"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(metadata["issuer"], base);
    assert_eq!(metadata["code_challenge_methods_supported"], json!(["S256"]));
    assert_eq!(metadata["registration_endpoint"], format!("{base}/register"));
}

#[tokio::test]
async fn registers_public_clients_only() {
    let f = Fixture::new().await;
    for (body, error) in [
        (json!({ "redirect_uris": [] }), "invalid_redirect_uri"),
        (
            json!({ "redirect_uris": ["http://evil.example/cb"] }),
            "invalid_redirect_uri",
        ),
        (
            json!({ "redirect_uris": ["https://app.example/cb#x"] }),
            "invalid_redirect_uri",
        ),
        (
            json!({ "redirect_uris": [REDIRECT], "token_endpoint_auth_method": "client_secret_basic" }),
            "invalid_client_metadata",
        ),
        (
            json!({ "redirect_uris": [REDIRECT], "grant_types": ["implicit"] }),
            "invalid_client_metadata",
        ),
        (
            json!({ "redirect_uris": [REDIRECT], "response_types": ["token"] }),
            "invalid_client_metadata",
        ),
    ] {
        let (status, answer) = f.register(body.clone()).await;
        assert_eq!(
            (status, answer["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some(error)),
            "{body}"
        );
    }
    let (status, answer) = f
        .register(json!({ "client_name": "Zupflow", "redirect_uris": [REDIRECT, "https://app.example/cb"] }))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(answer["client_id"].as_str().unwrap().starts_with("tbc_"));
    assert_eq!(answer["token_endpoint_auth_method"], "none");
    assert!(answer.get("client_secret").is_none());
    assert_eq!(f.running.taskboard.registered_clients().len(), 1);
}

#[tokio::test]
async fn never_redirects_to_an_unknown_client_or_address() {
    let f = Fixture::new().await;
    let client = f.client().await;
    let mut unknown = f.params(&client);
    unknown[1].1 = "tbc_nobody".to_owned();
    let (status, location, page) = f.visit(&unknown).await;
    assert_eq!((status, location), (StatusCode::BAD_REQUEST, None));
    assert!(page.contains("Unknown app"));
    for redirect in [
        "http://localhost:8080/connect/oauth/callback/",
        "https://evil.example/cb",
    ] {
        let mut elsewhere = f.params(&client);
        elsewhere[2].1 = redirect.to_owned();
        let (status, location, _) = f.visit(&elsewhere).await;
        assert_eq!((status, location), (StatusCode::BAD_REQUEST, None), "{redirect}");
    }
}

#[tokio::test]
async fn answers_a_bad_request_at_the_client() {
    let f = Fixture::new().await;
    let client = f.client().await;
    let mcp = f.running.taskboard.mcp_url();
    let cases: Vec<(&str, Option<&str>, &str)> = vec![
        ("response_type", Some("token"), "unsupported_response_type"),
        ("state", None, "invalid_request"),
        ("code_challenge_method", Some("plain"), "invalid_request"),
        ("code_challenge", None, "invalid_request"),
        ("resource", None, "invalid_request"),
        ("resource", Some("https://elsewhere.example/mcp"), "invalid_target"),
        ("scope", Some("tasks:read admin"), "invalid_scope"),
    ];
    for (name, value, error) in cases {
        let mut params: Vec<(&str, String)> = f.params(&client).into_iter().filter(|(key, _)| *key != name).collect();
        if let Some(value) = value {
            params.push((name, value.to_owned()));
        }
        let (status, location, _) = f.visit(&params).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{name}");
        let answer = query(&location.unwrap());
        assert_eq!(answer["error"], error, "{name}");
        assert_eq!(
            answer.get("state").map(String::as_str),
            (name != "state").then_some("s-1")
        );
        assert!(!answer.contains_key("code"));
    }
    let mut twice = f.params(&client);
    twice.push(("resource", mcp));
    let (_, location, _) = f.visit(&twice).await;
    assert_eq!(
        query(&location.unwrap())["error"],
        "invalid_request",
        "a parameter given twice"
    );
}

#[tokio::test]
async fn denying_answers_access_denied_and_spends_the_request() {
    let f = Fixture::new().await;
    let client = f.client().await;
    let (_, _, page) = f.visit(&f.params(&client)).await;
    assert!(page.contains("Zupflow wants to use your Taskboard"), "{page}");
    let answer = f.decide(&page, "deny").await;
    assert_eq!(answer["error"], "access_denied");
    assert_eq!(answer["state"], "s-1");
    assert!(!answer.contains_key("code"));
    // Answered once: the same page can't approve afterwards.
    let request = hidden(&page, "request");
    let again = f
        .http
        .post(f.url("/authorize"))
        .form(&[("request", request.as_str()), ("decision", "approve")])
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::BAD_REQUEST);
    // Another site can't answer for the person.
    let (_, _, page) = f.visit(&f.params(&client)).await;
    let request = hidden(&page, "request");
    let forged = f
        .http
        .post(f.url("/authorize"))
        .header("origin", "https://evil.example")
        .form(&[("request", request.as_str()), ("decision", "approve")])
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn exchanges_a_code_once_for_its_own_client_redirect_and_verifier() {
    let f = Fixture::new().await;
    let client = f.client().await;
    let other = f.client().await;
    let mcp = f.running.taskboard.mcp_url();
    for (change, error) in [
        (("redirect_uri", "https://app.example/cb"), "invalid_grant"),
        (
            ("code_verifier", "another-verifier-0123456789-0123456789-0123456789"),
            "invalid_grant",
        ),
        (("client_id", other.as_str()), "invalid_grant"),
        (("resource", "https://elsewhere.example/mcp"), "invalid_target"),
        (("grant_type", "password"), "unsupported_grant_type"),
    ] {
        let code = f.code(&client).await;
        let mut sent = vec![
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", REDIRECT),
            ("client_id", client.as_str()),
            ("code_verifier", VERIFIER),
            ("resource", mcp.as_str()),
        ];
        sent.retain(|(name, _)| *name != change.0);
        sent.push(change);
        let (status, answer) = f.token(&sent).await;
        assert_eq!(
            (status, answer["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some(error)),
            "{change:?}"
        );
    }
    // The resource is required, not only checked when given.
    let code = f.code(&client).await;
    let (status, answer) = f
        .token(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", REDIRECT),
            ("client_id", client.as_str()),
            ("code_verifier", VERIFIER),
        ])
        .await;
    assert_eq!(
        (status, answer["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_target"))
    );
    let (status, answer) = f
        .token(&[
            ("grant_type", "authorization_code"),
            ("code", "x"),
            ("client_id", "tbc_nobody"),
        ])
        .await;
    assert_eq!(
        (status, answer["error"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("invalid_client"))
    );

    // Once: a second exchange fails and revokes what the first one gave.
    let code = f.code(&client).await;
    let (status, tokens) = f.exchange(&client, &code).await;
    assert_eq!(status, StatusCode::OK, "{tokens}");
    assert_eq!(tokens["token_type"], "Bearer");
    assert_eq!(tokens["expires_in"], 3600);
    assert_eq!(tokens["scope"], "tasks:read tasks:write");
    let access = tokens["access_token"].as_str().unwrap();
    f.session(access).await;
    let (status, answer) = f.exchange(&client, &code).await;
    assert_eq!(
        (status, answer["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
    let (status, _, _) = f
        .mcp(
            Some(access),
            None,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "revoked with the code's reuse");
}

#[tokio::test]
async fn rotates_refresh_tokens_and_revokes_a_reused_one() {
    let f = Fixture::new().await;
    let client = f.client().await;
    let first = f.tokens(&client).await;
    let mcp = f.running.taskboard.mcp_url();
    let refresh = |token: String| {
        let client = client.clone();
        let mcp = mcp.clone();
        let f = &f;
        async move {
            f.token(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", &token),
                ("client_id", &client),
                ("resource", &mcp),
            ])
            .await
        }
    };
    // Without the resource: refused, and nothing is rotated.
    let (status, answer) = f
        .token(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", first["refresh_token"].as_str().unwrap()),
            ("client_id", &client),
        ])
        .await;
    assert_eq!(
        (status, answer["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_target"))
    );
    f.running.taskboard.expire_access_tokens();
    let (status, _, _) = f
        .mcp(
            first["access_token"].as_str(),
            None,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "expired");
    let (status, second) = refresh(first["refresh_token"].as_str().unwrap().to_owned()).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_ne!(second["refresh_token"], first["refresh_token"], "rotated");
    f.session(second["access_token"].as_str().unwrap()).await;
    // The old one again: refused, and the family revoked.
    let (status, answer) = refresh(first["refresh_token"].as_str().unwrap().to_owned()).await;
    assert_eq!(
        (status, answer["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
    let (status, answer) = refresh(second["refresh_token"].as_str().unwrap().to_owned()).await;
    assert_eq!(
        (status, answer["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
    let requests = f.running.taskboard.token_requests();
    assert_eq!(
        requests
            .iter()
            .map(|request| (
                request.grant_type.as_str(),
                request.resource.is_some(),
                request.error.as_deref()
            ))
            .collect::<Vec<_>>(),
        [
            ("authorization_code", true, None),
            ("refresh_token", false, Some("invalid_target")),
            ("refresh_token", true, None),
            ("refresh_token", true, Some("invalid_grant")),
            ("refresh_token", true, Some("invalid_grant")),
        ]
    );
}

#[tokio::test]
async fn serves_tools_to_a_session_with_a_live_token() {
    let f = Fixture::new().await;
    let client = f.client().await;
    let tokens = f.tokens(&client).await;
    let token = tokens["access_token"].as_str().unwrap();
    // Without a session, or with another client's: refused.
    let (status, _, _) = f
        .mcp(
            Some(token),
            None,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let other = f.client().await;
    let other_tokens = f.tokens(&other).await;
    let other_session = f.session(other_tokens["access_token"].as_str().unwrap()).await;
    let (status, _, _) = f
        .mcp(
            Some(token),
            Some(&other_session),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let session = f.session(token).await;
    // After initialize, the version agreed is named on every request.
    for version in [None, Some("2025-03-26")] {
        let mut request = f
            .http
            .post(f.running.taskboard.mcp_url())
            .bearer_auth(token)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", &session)
            .json(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
        if let Some(version) = version {
            request = request.header("mcp-protocol-version", version);
        }
        assert_eq!(
            request.send().await.unwrap().status(),
            StatusCode::BAD_REQUEST,
            "{version:?}"
        );
    }
    let (status, _, listed) = f
        .mcp(
            Some(token),
            Some(&session),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let annotations: Vec<(String, Value)> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| (tool["name"].as_str().unwrap().to_owned(), tool["annotations"].clone()))
        .collect();
    assert_eq!(annotations[0].0, "list_tasks");
    assert_eq!(annotations[0].1["readOnlyHint"], true);
    assert_eq!(annotations[1].0, "create_task");
    assert_eq!(annotations[1].1["destructiveHint"], false);
    assert_eq!(annotations[2].0, "delete_task");
    assert_eq!(annotations[2].1["destructiveHint"], true);
    let (status, _, _) = f
        .mcp(
            Some(token),
            Some(&session),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (_, created) = f
        .call(token, &session, "create_task", json!({ "title": "Write the report" }))
        .await;
    assert_eq!(created["result"]["structuredContent"]["task"]["id"], "TB-1");
    let (_, invalid) = f.call(token, &session, "create_task", json!({ "title": "" })).await;
    assert_eq!(invalid["result"]["isError"], true);
    let (_, listed) = f.call(token, &session, "list_tasks", json!({})).await;
    assert_eq!(listed["result"]["content"][0]["text"], "TB-1 [open] Write the report");
    let (_, unknown) = f.call(token, &session, "drop_tables", json!({})).await;
    assert_eq!(unknown["error"]["code"], -32602);
    // Boards are per client.
    assert!(f.running.taskboard.tasks(&other).is_empty());
    let (_, deleted) = f.call(token, &session, "delete_task", json!({ "id": "TB-1" })).await;
    assert_eq!(deleted["result"]["structuredContent"]["deleted"], "TB-1");
    let calls = f.running.taskboard.tool_calls();
    assert_eq!(calls.len(), 4, "an unknown tool is refused before anything is recorded");
    assert!(
        calls
            .iter()
            .all(|call| call.access_token == token && call.client_id == client)
    );

    // A revoked token is refused, saying why.
    f.running.taskboard.revoke_all_tokens();
    let (status, headers, _) = f
        .mcp(
            Some(token),
            Some(&session),
            json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        headers["www-authenticate"]
            .to_str()
            .unwrap()
            .contains(r#"error="invalid_token""#)
    );
}

#[tokio::test]
async fn refuses_a_tool_its_token_has_no_scope_for() {
    let f = Fixture::new().await;
    let client = f.client().await;
    let mut params = f.params(&client);
    params[4].1 = "tasks:read".to_owned();
    let (_, _, page) = f.visit(&params).await;
    let code = f.decide(&page, "approve").await["code"].clone();
    let (_, tokens) = f.exchange(&client, &code).await;
    let token = tokens["access_token"].as_str().unwrap();
    let session = f.session(token).await;
    let (status, _) = f.call(token, &session, "create_task", json!({ "title": "No" })).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = f.call(token, &session, "list_tasks", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let calls = f.running.taskboard.tool_calls();
    assert_eq!(
        calls.iter().map(|call| call.tool.as_str()).collect::<Vec<_>>(),
        ["list_tasks"],
        "a call refused for its scope never reached the tool"
    );
}

#[tokio::test]
async fn names_its_account() {
    let f = Fixture::new().await;
    let client = f.client().await;
    let tokens = f.tokens(&client).await;
    let me: Value = f
        .http
        .get(f.running.taskboard.identity_url())
        .bearer_auth(tokens["access_token"].as_str().unwrap())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        me,
        json!({ "sub": "tb-user-1", "name": "Robin Park", "email": "robin@taskboard.example" })
    );
    let refused = f.http.get(f.running.taskboard.identity_url()).send().await.unwrap();
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
}
