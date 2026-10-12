//! The hosted Connect page (decision 0005): served at `/connect/{reference}`
//! under a strict CSP, framed only by the environment's allowed origins,
//! everything it shows escaped, and one generic answer for every link that
//! can't be used.

#![allow(clippy::unwrap_used)]

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use base64::Engine;
use common::{Server, call};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha384};
use tower::ServiceExt;
use zupflow_server::RequestGuard;

/// An app name that would run if it weren't escaped.
const HOSTILE: &str = "<img src=x onerror=alert(1)>\"'&";
const EMBEDDER: &str = "https://app.northwind.example";
const RETURN: &str = "https://app.northwind.example/settings/integrations";
const VERIFY: &str = "https://verify.example/me";

struct Tenant {
    org: String,
    sandbox: String,
    app: String,
}

/// An organization with a Sandbox workspace and a ready token app named
/// [`HOSTILE`], verified at [`VERIFY`]; `origins` may embed its Sandbox.
async fn tenant(server: &Server, origins: &[&str]) -> Tenant {
    let org = server.organization("alice", "northwind").await;
    let sandbox = server.workspace("alice", &org, "acme", "sandbox").await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, body) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/apps")).body(json!({
                "kind": "mcp", "name": HOSTILE, "key": "tools", "description": "<b>bold</b>", "auth_type": "token",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let app = body["data"]["id"].as_str().unwrap().to_owned();
    let (status, body) = server
        .send(
            call("alice", Method::PATCH, format!("/v1/organizations/{org}/apps/{app}")).body(json!({ "auth": {
                "type": "token",
                "inputs": [{
                    "key": "api_key", "label": "API key <i>", "secret": true,
                    "help": "Settings > \"API\"", "placeholder": "tk_<x>", "pattern": null,
                }],
                "placement": { "location": "header", "name": "X-Api-Key", "prefix": "" },
                "verify_url": VERIFY, "instructions": "Paste <your> key.", "docs_url": "https://docs.example/keys?a=1&b=2",
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::PUT,
                format!("/v1/organizations/{org}/apps/{app}/environments/sandbox/configuration"),
            )
            .body(json!({ "mcp_server_url": "https://tools.example/mcp" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::PUT,
                format!("/v1/organizations/{org}/connect-settings/sandbox"),
            )
            .body(json!({ "return_urls": [RETURN], "allowed_origins": origins })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    Tenant { org, sandbox, app }
}

/// Starts a session; its ID and the reference at the end of its link.
async fn start(server: &Server, t: &Tenant, return_url: Value) -> (String, String) {
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/workspaces/{}/connect-sessions", t.sandbox),
            )
            .naming(&t.org)
            .body(json!({
                "app_id": t.app, "intent": "create", "label": "Main", "installation_id": null, "return_url": return_url,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let url = body["data"]["url"].as_str().unwrap();
    assert!(url.starts_with("http://localhost:8080/connect/"));
    (
        body["data"]["id"].as_str().unwrap().to_owned(),
        url.rsplit('/').next().unwrap().to_owned(),
    )
}

/// A browser opening `uri`: the status, headers and body.
async fn open(app: &axum::Router, uri: &str) -> (StatusCode, HeaderMap, String) {
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (parts.status, parts.headers, String::from_utf8(bytes.to_vec()).unwrap())
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).map(|value| value.to_str().unwrap())
}

async fn database(server: &Server) -> sqlx::SqlitePool {
    sqlx::SqlitePool::connect(&format!("sqlite:{}", server.directory.join("zupflow.sqlite").display()))
        .await
        .unwrap()
}

/// Moves a session's times so its link expired `ago_ms` ago.
async fn expire(server: &Server, session: &str, ago_ms: i64) {
    let pool = database(server).await;
    let now = zupflow_core::time::Timestamp::now().millis();
    let id = zupflow_core::ids::ConnectSessionId::parse(session).unwrap();
    sqlx::query("UPDATE connect_sessions SET created_at = ?, expires_at = ? WHERE id = ?")
        .bind(now - ago_ms - 15 * 60 * 1000)
        .bind(now - ago_ms)
        .bind(id.uuid())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

async fn submit(server: &Server, reference: &str, token: &str) -> (StatusCode, Value) {
    server
        .send(
            call(
                "nobody",
                Method::POST,
                format!("/v1/connect/sessions/{reference}/credentials"),
            )
            .body(json!({ "values": { "api_key": token } })),
        )
        .await
}

#[tokio::test]
async fn serves_a_pending_session_under_a_strict_policy() {
    let server = Server::new().await;
    let t = tenant(&server, &[EMBEDDER, "http://localhost:3000"]).await;
    let (_, reference) = start(&server, &t, Value::Null).await;

    let (status, headers, html) = open(&server.app, &format!("/connect/{reference}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(header(&headers, "content-type"), Some("text/html; charset=utf-8"));
    let policy = header(&headers, "content-security-policy").unwrap();
    for directive in [
        "default-src 'none'",
        "script-src 'self'",
        "img-src 'self'",
        "connect-src 'self'",
        "form-action 'self'",
        "base-uri 'none'",
        "frame-ancestors https://app.northwind.example http://localhost:3000",
        "require-trusted-types-for 'script'",
    ] {
        assert!(policy.contains(directive), "{directive} in {policy}");
    }
    // Styles: the page's own file, and only the one inline style it carries, by hash.
    let style = html
        .split("<style>")
        .nth(1)
        .and_then(|rest| rest.split("</style>").next())
        .unwrap();
    let hash = base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(style.as_bytes()));
    assert!(
        policy.contains(&format!("style-src 'self' 'sha256-{hash}';")),
        "{policy}"
    );
    assert!(!policy.contains("unsafe"));
    assert_eq!(header(&headers, "referrer-policy"), Some("no-referrer"));
    assert_eq!(header(&headers, "cache-control"), Some("no-store"));
    assert_eq!(header(&headers, "x-content-type-options"), Some("nosniff"));
    // Framed by the allowed origins, so no blanket refusal.
    assert_eq!(header(&headers, "x-frame-options"), None);

    // The reference authorizes the page; it never appears in it.
    assert!(!html.contains(&reference));
    // Everything the customer wrote is text, never markup.
    assert!(!html.contains("<img"));
    assert!(!html.contains("<your>"));
    assert!(!html.contains("<i>"));
    assert!(html.contains("Connect &lt;img src=x onerror=alert(1)&gt;&quot;&#39;&amp;</h1>"));
    assert!(html.contains("Paste &lt;your&gt; key."));
    assert!(html.contains("placeholder=\"tk_&lt;x&gt;\""));
    assert!(html.contains("href=\"https://docs.example/keys?a=1&amp;b=2\""));
    assert!(html.contains("<span class=\"env-badge\">Sandbox</span>"));
    // No input has a name, so without the script a submitted form carries no secret.
    let inputs: Vec<&str> = html
        .split("<input")
        .skip(1)
        .map(|rest| rest.split('>').next().unwrap())
        .collect();
    assert_eq!(inputs.len(), 1);
    assert!(inputs.iter().all(|input| !input.contains("name=")));
    // No inline script: the page's one script is its own file, pinned by its hash.
    assert_eq!(html.matches("<script").count(), 1);
    assert!(html.contains("<script src=\"/connect/assets/page.js?v="));
    assert!(
        !html.contains("data-failure"),
        "a token app's page has no consent to start"
    );
}

#[tokio::test]
async fn is_framed_by_nobody_without_allowed_origins() {
    let server = Server::new().await;
    let t = tenant(&server, &[]).await;
    let (_, reference) = start(&server, &t, Value::Null).await;
    let (status, headers, html) = open(&server.app, &format!("/connect/{reference}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        header(&headers, "content-security-policy")
            .unwrap()
            .contains("frame-ancestors 'none';")
    );
    assert_eq!(header(&headers, "x-frame-options"), Some("DENY"));
    assert!(html.contains("data-state=\"pending\""));
}

/// An unknown, malformed, expired or long-used link: the same page, the same
/// status, the same headers.
#[tokio::test]
async fn answers_every_unusable_link_with_the_same_page() {
    let server = Server::new().await;
    let t = tenant(&server, &[EMBEDDER]).await;

    let (expired, expired_reference) = start(&server, &t, Value::Null).await;
    expire(&server, &expired, 1).await;
    let (used, used_reference) = start(&server, &t, json!(RETURN)).await;
    assert_eq!(submit(&server, &used_reference, "tk_ok").await.0, StatusCode::OK);
    expire(&server, &used, zupflow_server::connect_page::OUTCOME_GRACE_MS + 1).await;
    let (cancelled, cancelled_reference) = start(&server, &t, Value::Null).await;
    let (status, _) = server
        .send(call(
            "nobody",
            Method::POST,
            format!("/v1/connect/sessions/{cancelled_reference}/cancel"),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    expire(&server, &cancelled, zupflow_server::connect_page::OUTCOME_GRACE_MS + 1).await;

    let unknown = "A".repeat(32);
    let too_long = "A".repeat(65);
    let mut answers = Vec::new();
    for reference in [
        unknown.as_str(),
        too_long.as_str(),
        "%FF%FE",
        &expired_reference,
        &used_reference,
        &cancelled_reference,
    ] {
        let (status, mut headers, html) = open(&server.app, &format!("/connect/{reference}")).await;
        headers.remove("date");
        answers.push((reference.to_owned(), status, headers, html));
    }
    let (_, status, headers, html) = &answers[0];
    assert_eq!(*status, StatusCode::NOT_FOUND);
    assert!(html.contains("This link isn&#39;t valid"));
    assert!(
        header(headers, "content-security-policy")
            .unwrap()
            .contains("frame-ancestors 'none';")
    );
    assert_eq!(header(headers, "x-frame-options"), Some("DENY"));
    for (reference, other_status, other_headers, other_html) in &answers[1..] {
        assert_eq!(
            (other_status, other_headers, other_html),
            (status, headers, html),
            "{reference}"
        );
    }

    // The hosted API follows the page's rule: the same 404 as no session.
    let (status, unknown_body) = hosted(&server, &unknown).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    for reference in [&expired_reference, &used_reference, &cancelled_reference] {
        assert_eq!(
            hosted(&server, reference).await,
            (StatusCode::NOT_FOUND, unknown_body.clone()),
            "{reference}"
        );
    }
}

/// The hosted API's read of a link: its status and body.
async fn hosted(server: &Server, reference: &str) -> (StatusCode, Value) {
    server
        .send(call("nobody", Method::GET, format!("/v1/connect/sessions/{reference}")))
        .await
}

/// A session that ended shows its outcome until its link's grace is over,
/// on the page and through the hosted API alike.
#[tokio::test]
async fn shows_a_cancelled_session_until_its_grace_is_over() {
    let server = Server::new().await;
    let t = tenant(&server, &[]).await;
    let (session, reference) = start(&server, &t, Value::Null).await;
    let (status, _) = server
        .send(call(
            "nobody",
            Method::POST,
            format!("/v1/connect/sessions/{reference}/cancel"),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    // Past the link's own expiry, within the grace.
    expire(&server, &session, 60 * 1000).await;
    let (status, _, html) = open(&server.app, &format!("/connect/{reference}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("data-state=\"cancelled\""));
    assert!(html.contains("<h1>Connect was cancelled</h1>"));
    assert!(html.contains("data-action=\"close\" hidden"));
    assert!(html.contains("data-close-note hidden"));
    let (status, body) = hosted(&server, &reference).await;
    assert_eq!((status, &body["data"]["status"]), (StatusCode::OK, &json!("cancelled")));

    expire(&server, &session, zupflow_server::connect_page::OUTCOME_GRACE_MS + 1).await;
    let (status, _, html) = open(&server.app, &format!("/connect/{reference}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(html.contains("This link isn&#39;t valid"));
    assert_eq!(hosted(&server, &reference).await.0, StatusCode::NOT_FOUND);
}

/// A session whose app is gone has nothing to show: the generic page, not
/// an outage.
#[tokio::test]
async fn answers_a_session_without_its_app_with_the_generic_page() {
    use sqlx::{Connection, Executor};
    let server = Server::new().await;
    let t = tenant(&server, &[]).await;
    let (_, reference) = start(&server, &t, Value::Null).await;
    let (_, _, generic) = open(&server.app, &format!("/connect/{}", "C".repeat(32))).await;
    // The schema keeps an app while a session names it; take it from under the session.
    let mut connection =
        sqlx::SqliteConnection::connect(&format!("sqlite:{}", server.directory.join("zupflow.sqlite").display()))
            .await
            .unwrap();
    connection.execute("PRAGMA foreign_keys = OFF").await.unwrap();
    sqlx::query("DELETE FROM apps WHERE id = ?")
        .bind(zupflow_core::ids::AppId::parse(&t.app).unwrap().uuid())
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();
    let (status, _, html) = open(&server.app, &format!("/connect/{reference}")).await;
    assert_eq!((status, html), (StatusCode::NOT_FOUND, generic));
}

#[tokio::test]
async fn shows_a_finished_session_and_sends_it_back_only_where_allowed() {
    let server = Server::new().await;
    let t = tenant(&server, &[EMBEDDER]).await;
    let (session, reference) = start(&server, &t, json!(RETURN)).await;
    assert_eq!(submit(&server, &reference, "tk_ok").await.0, StatusCode::OK);

    let (status, headers, html) = open(&server.app, &format!("/connect/{reference}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        header(&headers, "content-security-policy")
            .unwrap()
            .contains(&format!("frame-ancestors {EMBEDDER};"))
    );
    assert!(html.contains("data-state=\"completed\""));
    assert!(html.contains(&format!("data-session=\"{session}\"")));
    assert!(html.contains(&format!("data-origins=\"{EMBEDDER}\"")));
    assert!(html.contains(&format!("data-return=\"{RETURN}?zupflow_session={session}\"")));
    assert!(html.contains("data-close-note hidden"));
    assert!(html.contains("&lt;img src=x onerror=alert(1)&gt;&quot;&#39;&amp; connected"));

    // A return URL taken off the allowlist since sends nobody anywhere.
    let (status, _) = server
        .send(
            call(
                "alice",
                Method::PUT,
                format!("/v1/organizations/{}/connect-settings/sandbox", t.org),
            )
            .body(json!({ "return_urls": [], "allowed_origins": [] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, headers, html) = open(&server.app, &format!("/connect/{reference}")).await;
    assert!(!html.contains("data-return"));
    assert!(html.contains("data-origins=\"\""));
    assert!(html.contains("You can close this window."));
    assert_eq!(header(&headers, "x-frame-options"), Some("DENY"));
}

#[tokio::test]
async fn explains_a_failed_session_in_the_consoles_words() {
    let server = Server::new().await;
    let t = tenant(&server, &[]).await;
    let (session, reference) = start(&server, &t, Value::Null).await;
    let pool = database(&server).await;
    sqlx::query("UPDATE connect_sessions SET status = 'failed', failure_code = 'consent_denied' WHERE id = ?")
        .bind(zupflow_core::ids::ConnectSessionId::parse(&session).unwrap().uuid())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let (status, _, html) = open(&server.app, &format!("/connect/{reference}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("<h1>Access wasn&#39;t granted</h1>"));
    assert!(html.contains(
        "Nothing was connected. To connect &lt;img src=x onerror=alert(1)&gt;&quot;&#39;&amp;, start again from the \
         app that sent you here and allow access."
    ));
    assert!(html.contains("data-action=\"close\""));
}

/// A return URL that already names a session is told this one, once.
#[tokio::test]
async fn replaces_a_session_the_return_url_already_names() {
    let server = Server::new().await;
    let t = tenant(&server, &[]).await;
    let carrying = "https://app.northwind.example/back?zupflow_session=cs_stale&tab=apps";
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::PUT,
                format!("/v1/organizations/{}/connect-settings/sandbox", t.org),
            )
            .body(json!({ "return_urls": [carrying], "allowed_origins": [] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (session, reference) = start(&server, &t, json!(carrying)).await;
    assert_eq!(submit(&server, &reference, "tk_ok").await.0, StatusCode::OK);
    let (_, _, html) = open(&server.app, &format!("/connect/{reference}")).await;
    assert!(
        html.contains(&format!(
            "data-return=\"https://app.northwind.example/back?tab=apps&amp;zupflow_session={session}\""
        )),
        "{html}"
    );
    assert!(!html.contains("cs_stale"));
}

#[tokio::test]
async fn serves_its_own_assets_as_pinned() {
    let server = Server::new().await;
    let t = tenant(&server, &[]).await;
    let (_, reference) = start(&server, &t, Value::Null).await;
    let (_, headers, html) = open(&server.app, &format!("/connect/{reference}")).await;
    // The page itself is never cached; its files, each at its version's address, for a year.
    assert_eq!(header(&headers, "cache-control"), Some("no-store"));
    let linked = |path: &str| {
        let start = html.find(&format!("\"{path}?v=")).unwrap() + 1;
        html[start..].split('"').next().unwrap().to_owned()
    };
    for (path, content_type) in [
        ("/connect/assets/page.js", "text/javascript; charset=utf-8"),
        ("/connect/assets/page.css", "text/css; charset=utf-8"),
    ] {
        let url = linked(path);
        let (status, headers, body) = open(&server.app, &url).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(header(&headers, "content-type"), Some(content_type));
        assert_eq!(header(&headers, "x-content-type-options"), Some("nosniff"));
        assert_eq!(
            header(&headers, "cache-control"),
            Some("public, max-age=31536000, immutable")
        );
        let version: String = sha2::Sha256::digest(body.as_bytes())
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(url, format!("{path}?v={version}"));
        let integrity = format!(
            "sha384-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha384::digest(body.as_bytes()))
        );
        assert!(
            html.contains(&format!("href=\"{url}\" integrity=\"{integrity}\""))
                || html.contains(&format!("src=\"{url}\" integrity=\"{integrity}\"")),
            "{path}"
        );
    }
    let (status, headers, _) = open(&server.app, &linked("/connect/assets/icon.svg")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(header(&headers, "content-type"), Some("image/svg+xml"));
}

const PUBLIC: &str = "https://connect.northwind.example";
const CONSOLE: &str = "https://console.northwind.example";

fn from(origin: &str, method: &str, uri: &str, authorized: bool) -> Request<Body> {
    let mut request = Request::builder().method(method).uri(uri).header("Origin", origin);
    if authorized {
        request = request.header("Authorization", common::bearer("alice"));
    }
    request.body(Body::empty()).unwrap()
}

/// The page posts from the server's public origin; the guard admits that
/// origin for hosted Connect alone, and still refuses any other.
#[tokio::test]
async fn admits_the_public_origin_for_hosted_connect_only() {
    let server = Server::new().await;
    let t = tenant(&server, &[]).await;
    let (_, reference) = start(&server, &t, Value::Null).await;
    // As `main` composes it: the public URL, then the Console.
    let app = zupflow_server::app(
        server.state.clone(),
        RequestGuard::with_origins(&[])
            .with_public_url(PUBLIC)
            .with_console(CONSOLE),
    );
    let cancel = format!("/v1/connect/sessions/{reference}/cancel");
    let refused = app
        .clone()
        .oneshot(from("https://elsewhere.example", "POST", &cancel, false))
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);

    // A Console route from the public origin is refused, credential or not.
    let organization = format!("/v1/organizations/{}", t.org);
    for uri in [organization.clone(), format!("/v1/organizations/{}/apps", t.org)] {
        let response = app.clone().oneshot(from(PUBLIC, "GET", &uri, true)).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{uri}");
    }
    let response = app
        .clone()
        .oneshot(from(PUBLIC, "GET", &organization, true))
        .await
        .unwrap();
    assert!(response.headers().get("access-control-allow-origin").is_none());

    // Hosted Connect from the public origin: admitted, same-origin, so no CORS.
    let admitted = app.clone().oneshot(from(PUBLIC, "POST", &cancel, false)).await.unwrap();
    assert_eq!(admitted.status(), StatusCode::OK);
    assert!(admitted.headers().get("access-control-allow-origin").is_none());
    let preflight = Request::builder()
        .method("OPTIONS")
        .uri(&cancel)
        .header("Origin", PUBLIC)
        .header("Access-Control-Request-Method", "POST")
        .body(Body::empty())
        .unwrap();
    let preflight = app.clone().oneshot(preflight).await.unwrap();
    assert!(preflight.headers().get("access-control-allow-origin").is_none());

    // The Console keeps its CORS, and only it.
    let console = app
        .clone()
        .oneshot(from(CONSOLE, "GET", &organization, true))
        .await
        .unwrap();
    assert_eq!(console.status(), StatusCode::OK);
    assert_eq!(console.headers().get("access-control-allow-origin").unwrap(), CONSOLE);
}

/// Everything the server logs while a link is used, at every level.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The reference is a bearer secret: nothing logged while it's used names it.
#[tokio::test]
async fn never_logs_the_reference() {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    let _default = tracing::subscriber::set_default(subscriber);

    let server = Server::new().await;
    let t = tenant(&server, &[EMBEDDER]).await;
    let (_, reference) = start(&server, &t, json!(RETURN)).await;
    let unknown = "B".repeat(32);
    for path in [format!("/connect/{reference}"), format!("/connect/{unknown}")] {
        open(&server.app, &path).await;
    }
    // A provider that can't answer is logged; the link isn't.
    *server.providers.answer.lock().unwrap() = Ok(500);
    let (status, _) = submit(&server, &reference, "tk_ok").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    *server.providers.answer.lock().unwrap() = Ok(200);
    let (status, _) = server
        .send(
            call(
                "nobody",
                Method::POST,
                format!("/v1/connect/sessions/{reference}/credentials"),
            )
            .idempotent("page-1")
            .body(json!({ "values": { "api_key": "tk_ok" } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    open(&server.app, &format!("/connect/{reference}")).await;
    server
        .send(call(
            "nobody",
            Method::POST,
            format!("/v1/connect/sessions/{unknown}/cancel"),
        ))
        .await;

    let logged = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(logged.contains("connect.verify_unexpected_status"), "{logged}");
    assert!(!logged.contains(&reference));
    assert!(!logged.contains(&unknown));
}

// --- OAuth ----------------------------------------------------------------------

const AUTHORIZE: &str = "https://accounts.example/authorize";
const TOKEN: &str = "https://accounts.example/token";
const IDENTITY: &str = "https://accounts.example/me";
/// A scope that would be markup if it weren't escaped, under Google's prefix.
const SCOPE: &str = "https://www.googleapis.com/auth/<b>\"files\"";

/// A ready OAuth catalog app named [`HOSTILE`], asking for [`SCOPE`].
async fn oauth_app(server: &Server, t: &Tenant) -> String {
    let org = &t.org;
    let (status, created) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/apps")).body(json!({
                "kind": "mcp", "name": HOSTILE, "key": "tracker", "description": "", "auth_type": "oauth2",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let app = created["data"]["id"].as_str().unwrap().to_owned();
    let (status, body) = server
        .send(
            call("alice", Method::PATCH, format!("/v1/organizations/{org}/apps/{app}")).body(json!({ "auth": {
                "type": "oauth2", "authorize_url": AUTHORIZE, "token_url": TOKEN, "identity_url": IDENTITY,
                "available_scopes": [{ "name": SCOPE, "description": "" }], "pkce": true,
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::PUT,
                format!("/v1/organizations/{org}/apps/{app}/environments/sandbox/configuration"),
            )
            .body(json!({
                "mcp_server_url": "https://tracker.example/mcp", "oauth_client_id": "client-1",
                "oauth_client_secret": "not-a-real-secret-for-tests", "oauth_scopes": [SCOPE],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    app
}

/// Starts an OAuth session for `app` (a reconnect of `installation` when
/// given): its ID and reference.
async fn start_oauth(server: &Server, t: &Tenant, app: &str, installation: Option<&str>) -> (String, String) {
    let intent = if installation.is_some() { "reconnect" } else { "create" };
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/workspaces/{}/connect-sessions", t.sandbox),
            )
            .naming(&t.org)
            .body(json!({
                "app_id": app, "intent": intent, "label": "Tracker", "installation_id": installation,
                "return_url": null,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let url = body["data"]["url"].as_str().unwrap();
    (
        body["data"]["id"].as_str().unwrap().to_owned(),
        url.rsplit('/').next().unwrap().to_owned(),
    )
}

/// Starts the consent: the `state` the provider sends back.
async fn consent_state(server: &Server, reference: &str) -> String {
    let (status, body) = server
        .send(call(
            "nobody",
            Method::POST,
            format!("/v1/connect/sessions/{reference}/authorize"),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let url = url::Url::parse(body["data"]["authorization_url"].as_str().unwrap()).unwrap();
    url.query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .unwrap()
}

#[tokio::test]
async fn offers_an_oauth_apps_consent_with_everything_escaped() {
    let server = Server::new().await;
    let t = tenant(&server, &[]).await;
    let app = oauth_app(&server, &t).await;
    let (_, reference) = start_oauth(&server, &t, &app, None).await;
    let (status, _, html) = open(&server.app, &format!("/connect/{reference}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!html.contains("<b>"));
    assert!(!html.contains("<img"));
    assert!(html.contains("<title>Connect &lt;img src=x onerror=alert(1)&gt;&quot;&#39;&amp;</title>"));
    // The scope shown without Google's prefix, and whole in its title.
    assert!(html.contains(
        "<li title=\"https://www.googleapis.com/auth/&lt;b&gt;&quot;files&quot;\">&lt;b&gt;&quot;files&quot;</li>"
    ));
    assert!(html.contains("data-action=\"authorize\""));
    assert!(html.contains("Continue to &lt;img src=x onerror=alert(1)&gt;&quot;&#39;&amp;</span>"));
    // What a refused start says, written by the server for this app.
    assert!(html.contains(
        "<p data-failure=\"app_not_eligible\">&lt;img src=x onerror=alert(1)&gt;&quot;&#39;&amp; can&#39;t be \
         connected here anymore"
    ));
    assert!(html.contains(
        "<p data-failure=\"configuration_not_ready\">Nothing was connected. Start again from the app that sent you \
         here.</p>"
    ));
    assert!(html.contains("<p data-failure=\"default\">"));
    // No token form.
    assert!(!html.contains("<form"));
    assert!(!html.contains("<input"));
}

/// Decision 0037's round trip: the provider sends the browser to the
/// callback, the callback back to the link, and the link shows how it ended
/// until its grace is over.
#[tokio::test]
async fn shows_the_outcome_the_oauth_callback_sends_back() {
    let server = Server::new().await;
    let t = tenant(&server, &[]).await;
    let app = oauth_app(&server, &t).await;

    // Allowed: completed.
    let (completed, reference) = start_oauth(&server, &t, &app, None).await;
    let state = consent_state(&server, &reference).await;
    server.providers.script(
        TOKEN,
        200,
        json!({ "access_token": "at-1", "refresh_token": "rt-1", "token_type": "Bearer", "expires_in": 3600 }),
    );
    server.providers.script(
        IDENTITY,
        200,
        json!({ "sub": "u-42", "name": "Jordan", "email": "<b>jordan</b>@people.example" }),
    );
    let (status, location, _) = server
        .visit(&format!("/connect/oauth/callback?code=c-1&state={state}"))
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let back = location.unwrap();
    assert_eq!(back, format!("http://localhost:8080/connect/{reference}"));
    let (status, _, html) = open(&server.app, &format!("/connect/{reference}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("data-state=\"completed\""));
    assert!(html.contains("&lt;img src=x onerror=alert(1)&gt;&quot;&#39;&amp; connected</h1>"));
    let installation = hosted(&server, &reference).await.1["data"]["installation_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Denied: consent_denied, in the Console's words.
    let (denied, denied_reference) = start_oauth(&server, &t, &app, None).await;
    let state = consent_state(&server, &denied_reference).await;
    let (status, location, _) = server
        .visit(&format!("/connect/oauth/callback?error=access_denied&state={state}"))
        .await;
    assert_eq!(
        (status, location),
        (
            StatusCode::SEE_OTHER,
            Some(format!("http://localhost:8080/connect/{denied_reference}"))
        )
    );
    let (status, _, html) = open(&server.app, &format!("/connect/{denied_reference}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("data-state=\"failed\""));
    assert!(html.contains("<h1>Access wasn&#39;t granted</h1>"));

    // Within the grace past the link's expiry, still shown; after it, the generic page.
    for (session, reference) in [(&completed, &reference), (&denied, &denied_reference)] {
        expire(
            &server,
            session,
            zupflow_server::connect_page::OUTCOME_GRACE_MS - 60 * 1000,
        )
        .await;
        assert_eq!(
            open(&server.app, &format!("/connect/{reference}")).await.0,
            StatusCode::OK
        );
        expire(&server, session, zupflow_server::connect_page::OUTCOME_GRACE_MS + 1).await;
        let (status, _, html) = open(&server.app, &format!("/connect/{reference}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(html.contains("This link isn&#39;t valid"));
    }

    // A reconnect names the account linked, escaped.
    let (_, reconnect) = start_oauth(&server, &t, &app, Some(&installation)).await;
    let (status, _, html) = open(&server.app, &format!("/connect/{reconnect}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("<h1>Reconnect &lt;img src=x onerror=alert(1)&gt;&quot;&#39;&amp;</h1>"));
    assert!(
        html.contains("Use &lt;b&gt;jordan&lt;/b&gt;@people.example \u{2014} the account already linked."),
        "{html}"
    );
    assert!(!html.contains("<b>"));
}

/// A Production session's page carries no Sandbox badge.
#[tokio::test]
async fn shows_no_sandbox_badge_in_production() {
    let server = Server::new().await;
    let t = tenant(&server, &[]).await;
    let production = server.workspace("alice", &t.org, "live", "production").await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::PUT,
                format!(
                    "/v1/organizations/{}/apps/{}/environments/production/configuration",
                    t.org, t.app
                ),
            )
            .body(json!({ "mcp_server_url": "https://tools.example/mcp" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, key) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{}/api-keys", t.org))
                .body(json!({ "name": "Server", "environment": "production" })),
        )
        .await;
    let key = key["data"]["secret"].as_str().unwrap().to_owned();
    let (status, body) = server
        .send(
            call(
                &key,
                Method::POST,
                format!("/v1/workspaces/{production}/connect-sessions"),
            )
            .body(json!({
                "app_id": t.app, "intent": "create", "label": "Main", "installation_id": null, "return_url": null,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let reference = body["data"]["url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned();
    let (status, _, html) = open(&server.app, &format!("/connect/{reference}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("data-state=\"pending\""));
    assert!(!html.contains("env-badge"));
    assert!(!html.contains("Sandbox"));

    // The Sandbox page does carry it.
    let (_, sandbox) = start(&server, &t, Value::Null).await;
    let (_, _, html) = open(&server.app, &format!("/connect/{sandbox}")).await;
    assert!(html.contains("<span class=\"env-badge\">Sandbox</span>"));
}
