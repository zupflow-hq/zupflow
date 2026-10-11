//! Taskboard's pages: the consent page, error pages, and a home page for
//! poking at it by hand.

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};

use crate::oauth::same_origin;
use crate::{ACCOUNT_EMAIL, ACCOUNT_NAME, SCOPES, Taskboard};

/// What went wrong, on a page: never a redirect to a client we can't trust.
pub(crate) enum Problem {
    UnknownClient,
    BadRedirect,
    UnknownRequest,
    BadDecision,
    ForeignOrigin,
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            c => escaped.push(c),
        }
    }
    escaped
}

const STYLE: &str = "
:root { color-scheme: light dark; --bg: #f6f7f9; --card: #fff; --text: #16181d; --muted: #5b616e; --line: #dde1e7; --accent: #2f5fd0; }
@media (prefers-color-scheme: dark) { :root { --bg: #111317; --card: #1a1d23; --text: #eceef2; --muted: #a3a9b5; --line: #2c3038; --accent: #7aa2ff; } }
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--text); font: 15px/1.5 system-ui, sans-serif; }
main { max-width: 440px; margin: 48px auto; padding: 0 16px; }
.card { background: var(--card); border: 1px solid var(--line); border-radius: 12px; padding: 24px; }
h1 { font-size: 20px; margin: 0 0 8px; }
p, li { color: var(--muted); }
.brand { font-weight: 600; color: var(--accent); margin-bottom: 16px; }
.actions { display: flex; gap: 8px; margin-top: 20px; }
button { flex: 1; font: inherit; padding: 10px 12px; border-radius: 8px; border: 1px solid var(--line); background: var(--card); color: var(--text); cursor: pointer; }
button.primary { background: var(--accent); border-color: var(--accent); color: #fff; }
table { width: 100%; border-collapse: collapse; } td { padding: 4px 0; color: var(--muted); }
";

fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>{title}</title><style>{STYLE}</style></head><body><main><div class=\"brand\">Taskboard</div><div class=\"card\">{body}</div></main></body></html>",
        title = escape(title)
    );
    let mut response = (status, Html(html)).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    response
}

pub(crate) fn problem(problem: Problem) -> Response {
    let (title, text) = match problem {
        Problem::UnknownClient => (
            "Unknown app",
            "The app that sent you here isn't registered with Taskboard. Go back and start again from the app.",
        ),
        Problem::BadRedirect => (
            "Unexpected return address",
            "The app asked to return somewhere it didn't register, so Taskboard won't send you there.",
        ),
        Problem::UnknownRequest => (
            "This request expired",
            "This sign-in request was already answered or is too old. Start again from the app.",
        ),
        Problem::BadDecision => (
            "Choose allow or deny",
            "Taskboard didn't understand the answer. Start again from the app.",
        ),
        Problem::ForeignOrigin => (
            "Not from Taskboard",
            "That answer came from another site, so Taskboard ignored it.",
        ),
    };
    page(
        StatusCode::BAD_REQUEST,
        title,
        &format!("<h1>{}</h1><p>{}</p>", escape(title), escape(text)),
    )
}

/// The consent page: who's asking, for what, as whom.
pub(crate) fn consent(request: &str, client_name: Option<&str>, scopes: &[String]) -> Response {
    let app = client_name.unwrap_or("An app");
    let items: String = scopes
        .iter()
        .map(|scope| {
            let described = SCOPES
                .iter()
                .find(|(name, _)| name == scope)
                .map_or(scope.as_str(), |(_, description)| description);
            format!("<li>{}</li>", escape(described))
        })
        .collect();
    let body = format!(
        "<h1>{app} wants to use your Taskboard</h1>\
         <p>Signed in as {name} ({email}). It will be able to:</p><ul>{items}</ul>\
         <form method=\"post\" action=\"/authorize\">\
         <input type=\"hidden\" name=\"request\" value=\"{request}\">\
         <div class=\"actions\">\
         <button type=\"submit\" name=\"decision\" value=\"deny\">Deny</button>\
         <button type=\"submit\" name=\"decision\" value=\"approve\" class=\"primary\">Allow</button>\
         </div></form>",
        app = escape(app),
        name = escape(ACCOUNT_NAME),
        email = escape(ACCOUNT_EMAIL),
        request = escape(request),
    );
    page(StatusCode::OK, "Allow access to Taskboard", &body)
}

/// What Taskboard holds, with buttons to expire or revoke every token.
pub(crate) async fn home(State(board): State<Taskboard>) -> Response {
    let clients = board.registered_clients();
    let rows: String = clients
        .iter()
        .map(|client| {
            format!(
                "<tr><td>{}</td><td>{} tasks</td></tr>",
                escape(client.client_name.as_deref().unwrap_or(&client.client_id)),
                board.tasks(&client.client_id).len()
            )
        })
        .collect();
    let calls = board.tool_calls().len();
    let body = format!(
        "<h1>Taskboard</h1>\
         <p>A fake MCP app for development. MCP server: <code>{mcp}</code></p>\
         <table>{rows}</table><p>{count} clients registered, {calls} tool calls served.</p>\
         <form method=\"post\" action=\"/dev/expire-access-tokens\"><div class=\"actions\"><button type=\"submit\">Expire access tokens</button></div></form>\
         <form method=\"post\" action=\"/dev/revoke-tokens\"><div class=\"actions\"><button type=\"submit\">Revoke every token</button></div></form>",
        mcp = escape(&board.mcp_url()),
        count = clients.len(),
    );
    page(StatusCode::OK, "Taskboard", &body)
}

fn done(text: &str) -> Response {
    page(
        StatusCode::OK,
        "Taskboard",
        &format!("<h1>Done</h1><p>{}</p><p><a href=\"/\">Back</a></p>", escape(text)),
    )
}

pub(crate) async fn expire_access_tokens(State(board): State<Taskboard>, headers: HeaderMap) -> Response {
    if !same_origin(&board, &headers) {
        return problem(Problem::ForeignOrigin);
    }
    board.expire_access_tokens();
    done("Every access token issued so far has expired. Refresh tokens still work.")
}

pub(crate) async fn revoke_tokens(State(board): State<Taskboard>, headers: HeaderMap) -> Response {
    if !same_origin(&board, &headers) {
        return problem(Problem::ForeignOrigin);
    }
    board.revoke_all_tokens();
    done("Every token issued so far is revoked: apps must sign in again.")
}
