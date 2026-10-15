//! The hosted Connect page (decision 0005): what a session's link opens, at
//! `{ZUPFLOW_PUBLIC_URL}/connect/{reference}`. A small page of its own, with
//! none of the Console's code: the server renders every state, and one
//! script (`connect-page/page.js`, served from here) sends what the person
//! does to the hosted Connect API on the same origin.
//!
//! - **The reference is a bearer secret.** It is never logged, never written
//!   into the page (the script reads it from its own address), and never sent
//!   on as a referrer.
//! - **No oracle.** A reference that names no session, a session that
//!   expired, and one used more than [`OUTCOME_GRACE_MS`] past its link's
//!   expiry answer the same generic page, byte for byte, and the hosted
//!   API (`GET /v1/connect/sessions/{reference}`) the same `404` as no
//!   session (`shown`). Until then a used link shows its outcome, because
//!   the OAuth callback sends the browser back here to read it. A session
//!   whose app is gone shows the generic page too; only our own store
//!   failing answers `503`.
//! - **Strict CSP.** Nothing loads but this page's own script, stylesheet
//!   and icon (each pinned by its integrity hash), plus, for the app's icon
//!   color, one inline style allowed by its hash; requests go to this origin
//!   only. Only the session's environment's `allowed_origins` may frame it
//!   (`http://localhost` only in Sandbox), and only they are told when it
//!   finishes (`postMessage`): a hint the embedder confirms through the API.
//! - **Cached apart.** The page is never cached; its files are, for a year,
//!   each at an address naming its content's version.
//! - **Everything shown is escaped**: app names, descriptions, inputs and
//!   accounts are the customer's or the provider's text.

use std::fmt::Write as _;
use std::sync::LazyLock;

use axum::Router;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256, Sha384};
use zupflow_core::apps::{App, AppError, AuthTemplate, TokenInput};
use zupflow_core::connect::{ConnectIntent, ConnectSession, ConnectSessionStatus};
use zupflow_core::environment::Environment;
use zupflow_core::installations::{ConnectedAccount, InstalledApp};
use zupflow_core::time::Timestamp;

use crate::auth::AppState;
use crate::connect::{attributed, expected_account, secure_enough};

/// How long past its link's expiry a used link still shows its outcome:
/// long enough to come back from a provider's consent screen and read it.
pub const OUTCOME_GRACE_MS: i64 = 5 * 60 * 1000;

const SCRIPT: &str = include_str!("../connect-page/page.js");
const STYLESHEET: &str = include_str!("../connect-page/page.css");
const ICON: &str = include_str!("../connect-page/icon.svg");

const SCRIPT_PATH: &str = "/connect/assets/page.js";
const STYLESHEET_PATH: &str = "/connect/assets/page.css";
const ICON_PATH: &str = "/connect/assets/icon.svg";

/// How long a browser keeps an asset: a year. Each is linked with its
/// content's version in the query (and the script and stylesheet with their
/// integrity), so a new build is a new address.
const ASSET_CACHE: &str = "public, max-age=31536000, immutable";

/// What a link shows. A pending session, and one that ended, until
/// [`OUTCOME_GRACE_MS`] past its link's expiry; nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shown {
    Pending,
    Completed,
    Failed,
    Cancelled,
}

impl Shown {
    fn status(self) -> ConnectSessionStatus {
        match self {
            Self::Pending => ConnectSessionStatus::Pending,
            Self::Completed => ConnectSessionStatus::Completed,
            Self::Failed => ConnectSessionStatus::Failed,
            Self::Cancelled => ConnectSessionStatus::Cancelled,
        }
    }
}

/// What the session's link shows at `now`, or `None` when it shows nothing:
/// it expired unused, or ended longer ago than the grace allows. The page
/// and the hosted API (`GET /v1/connect/sessions/{reference}`) both answer
/// `None` exactly as they answer a reference that names no session.
pub(crate) fn shown(session: &ConnectSession, now: Timestamp) -> Option<Shown> {
    let within_grace = now.millis() < session.expires_at.millis().saturating_add(OUTCOME_GRACE_MS);
    let ended = |shown| within_grace.then_some(shown);
    match session.status_at(now) {
        ConnectSessionStatus::Pending => Some(Shown::Pending),
        ConnectSessionStatus::Completed => ended(Shown::Completed),
        ConnectSessionStatus::Failed => ended(Shown::Failed),
        ConnectSessionStatus::Cancelled => ended(Shown::Cancelled),
        ConnectSessionStatus::Expired => None,
    }
}

/// Why a shown session couldn't be rendered.
enum Unrendered {
    /// Its app is gone: the link has nothing left to show.
    Gone,
    /// Our store failed: nothing about the link is known.
    Unavailable,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/connect/{reference}", get(page))
        .route(SCRIPT_PATH, get(script))
        .route(STYLESHEET_PATH, get(stylesheet))
        .route(ICON_PATH, get(icon))
}

// --- Headers ------------------------------------------------------------------

/// A Subresource Integrity value for an asset: its SHA-384, base64.
fn integrity(content: &str) -> String {
    format!("sha384-{}", STANDARD.encode(Sha384::digest(content.as_bytes())))
}

/// An asset's version, for its address: the start of its SHA-256, hex.
fn version(content: &str) -> String {
    Sha256::digest(content.as_bytes())
        .iter()
        .take(8)
        .fold(String::with_capacity(16), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

static SCRIPT_INTEGRITY: LazyLock<String> = LazyLock::new(|| integrity(SCRIPT));
static STYLESHEET_INTEGRITY: LazyLock<String> = LazyLock::new(|| integrity(STYLESHEET));
static SCRIPT_URL: LazyLock<String> = LazyLock::new(|| format!("{SCRIPT_PATH}?v={}", version(SCRIPT)));
static STYLESHEET_URL: LazyLock<String> = LazyLock::new(|| format!("{STYLESHEET_PATH}?v={}", version(STYLESHEET)));
static ICON_URL: LazyLock<String> = LazyLock::new(|| format!("{ICON_PATH}?v={}", version(ICON)));

/// The origins that may frame the page and hear from it: the environment's
/// `allowed_origins`, each re-read as exactly an origin, so nothing else
/// (a `;`, a space, a path) can reach the policy or a `postMessage` target,
/// and each as secure as the environment demands (`http://localhost` only in
/// Sandbox).
fn embedding_origins(allowed: &[String], environment: Environment) -> Vec<String> {
    allowed
        .iter()
        .filter(|origin| {
            url::Url::parse(origin).is_ok_and(|url| {
                secure_enough(&url, environment) && url.origin().ascii_serialization() == origin.as_str()
            })
        })
        .cloned()
        .collect()
}

/// The page's Content-Security-Policy: its own files only, the inline style
/// whose hash is `style` if any, requests and forms to this origin, framed
/// only by `ancestors` (by nobody when empty), and no script sinks at all.
pub fn content_security_policy(ancestors: &[String], style: Option<&str>) -> String {
    let style = style.map(|hash| format!(" 'sha256-{hash}'")).unwrap_or_default();
    let ancestors = if ancestors.is_empty() {
        "'none'".to_owned()
    } else {
        ancestors.join(" ")
    };
    format!(
        "default-src 'none'; script-src 'self'; style-src 'self'{style}; img-src 'self'; \
         connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors {ancestors}; \
         require-trusted-types-for 'script'; trusted-types 'none'"
    )
}

/// The headers every response here carries: never sniffed, never sent on as
/// a referrer, and cached only as `cache` says.
fn secured(mut response: Response, policy: &str, framed_by_anyone: bool, cache: &'static str) -> Response {
    let headers = response.headers_mut();
    match HeaderValue::from_str(policy) {
        Ok(value) => {
            headers.insert(header::CONTENT_SECURITY_POLICY, value);
        }
        // Built from checked origins and base64 only; refuse to serve rather than serve unprotected.
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if !framed_by_anyone {
        // For browsers older than `frame-ancestors`.
        headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    }
    response
}

fn asset(content: &'static str, content_type: &'static str) -> Response {
    let response = (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            // For the page alone. Not on the page itself: an embedder with
            // COEP couldn't frame it.
            (
                HeaderName::from_static("cross-origin-resource-policy"),
                HeaderValue::from_static("same-origin"),
            ),
        ],
        content,
    )
        .into_response();
    secured(
        response,
        "default-src 'none'; frame-ancestors 'none'",
        false,
        ASSET_CACHE,
    )
}

async fn script() -> Response {
    asset(SCRIPT, "text/javascript; charset=utf-8")
}

async fn stylesheet() -> Response {
    asset(STYLESHEET, "text/css; charset=utf-8")
}

async fn icon() -> Response {
    asset(ICON, "image/svg+xml")
}

// --- The page -----------------------------------------------------------------

/// Text for HTML, in content or a double-quoted attribute.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

async fn page(State(state): State<AppState>, reference: Result<Path<String>, PathRejection>) -> Response {
    let Ok(Path(reference)) = reference else {
        return generic();
    };
    let session = match attributed(&state, &reference).await {
        Ok(session) => session,
        Err(failure) if failure.0.status == 404 => return generic(),
        // The store has logged why; nothing here says anything of the reference.
        Err(_) => return unavailable(),
    };
    let Some(shown) = shown(&session, Timestamp::now()) else {
        return generic();
    };
    match render_session(&state, &session, shown).await {
        Ok(response) => response,
        Err(Unrendered::Gone) => generic(),
        Err(Unrendered::Unavailable) => unavailable(),
    }
}

/// The session's app, in the session's own scope.
async fn session_app(state: &AppState, session: &ConnectSession) -> Result<App, Unrendered> {
    // A channel session has no app: the page shows its generic face.
    let Some(installed) = session.app else {
        return Err(Unrendered::Gone);
    };
    let found = match installed {
        InstalledApp::Catalog(id) => state.apps.find_organization_app(session.organization_id, id).await,
        InstalledApp::Workspace(id) => {
            state
                .apps
                .find_workspace_app(session.organization_id, session.workspace_id, id)
                .await
        }
    };
    match found {
        Ok(Some(app)) => Ok(app),
        Ok(None) | Err(AppError::NotFound) => {
            tracing::warn!(event = "connect_page.app_gone", session_id = %session.id);
            Err(Unrendered::Gone)
        }
        Err(_) => {
            tracing::error!(event = "connect_page.app_unavailable", session_id = %session.id);
            Err(Unrendered::Unavailable)
        }
    }
}

/// What the page knows of where it lives: who may frame it and hear from it,
/// and where a finished session sends the person back to.
struct Embedding {
    origins: Vec<String>,
    /// The session's return URL, if its environment still allows it exactly,
    /// told which session ended.
    back: Option<String>,
}

async fn embedding(state: &AppState, session: &ConnectSession) -> Result<Embedding, Unrendered> {
    let settings = state
        .connect
        .find_settings(session.organization_id, session.environment)
        .await
        .map_err(|_| {
            tracing::error!(event = "connect_page.settings_unavailable", session_id = %session.id);
            Unrendered::Unavailable
        })?;
    // Checked when the session started; checked again, so a URL taken off
    // the allowlist since sends nobody anywhere.
    let back = session
        .return_url
        .as_ref()
        .filter(|url| settings.return_urls.contains(url))
        .and_then(|url| url::Url::parse(url).ok())
        .filter(|url| secure_enough(url, session.environment))
        .map(|url| with_session(url, &session.id.to_string()));
    Ok(Embedding {
        origins: embedding_origins(&settings.allowed_origins, session.environment),
        back,
    })
}

/// The return URL told which session ended: one `zupflow_session`, ours,
/// in place of any it carried.
fn with_session(mut url: url::Url, session_id: &str) -> String {
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .into_owned()
        .filter(|(key, _)| key != "zupflow_session")
        .collect();
    url.query_pairs_mut()
        .clear()
        .extend_pairs(kept)
        .append_pair("zupflow_session", session_id);
    url.to_string()
}

async fn render_session(state: &AppState, session: &ConnectSession, shown: Shown) -> Result<Response, Unrendered> {
    let app = session_app(state, session).await?;
    let embedding = embedding(state, session).await?;
    let (title, card) = match shown {
        Shown::Pending => {
            let account = expected_account(state, session).await.map_err(|_| {
                tracing::error!(event = "connect_page.account_unavailable", session_id = %session.id);
                Unrendered::Unavailable
            })?;
            let verb = match session.intent {
                ConnectIntent::Create => "Connect",
                ConnectIntent::Reconnect => "Reconnect",
            };
            (
                format!("{verb} {}", app.name),
                pending(&app, session, verb, account.as_ref()),
            )
        }
        Shown::Completed => (
            format!("{} connected", app.name),
            finished(session, shown, &embedding, &completed(&app, embedding.back.is_some())),
        ),
        Shown::Failed | Shown::Cancelled => {
            let (heading, body) = if shown == Shown::Cancelled {
                ("Connect was cancelled".to_owned(), "Nothing was connected.".to_owned())
            } else {
                hosted_failure(&app, session.failure_code.as_deref())
            };
            (
                heading.clone(),
                finished(
                    session,
                    shown,
                    &embedding,
                    &ended(&heading, &body, embedding.back.as_deref()),
                ),
            )
        }
    };
    let color = icon_color(&app.icon.color);
    let style = format!(".app-icon{{background-color:{color}}}");
    let hash = STANDARD.encode(Sha256::digest(style.as_bytes()));
    let html = document(&title, Some(&style), &card);
    let policy = content_security_policy(&embedding.origins, Some(&hash));
    let response = (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        html,
    )
        .into_response();
    Ok(secured(response, &policy, !embedding.origins.is_empty(), "no-store"))
}

/// The one answer for every link that can't be used: nothing in it, its
/// status or its headers depends on the reference.
fn generic() -> Response {
    let card = format!(
        "<div class=\"card\" data-state=\"generic\">{}</div>",
        outcome(
            "failure",
            X_CIRCLE,
            "This link isn't valid",
            "Connect links work once and expire after 15 minutes. Start again from the app that sent you here.",
            "",
        )
    );
    let response = (
        StatusCode::NOT_FOUND,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        document("Connect", None, &card),
    )
        .into_response();
    secured(response, &content_security_policy(&[], None), false, "no-store")
}

/// A bad callback state is a browser outcome, with the hosted page's CSP
/// and no request text or session information reflected in it.
pub(crate) fn invalid_oauth_state() -> Response {
    let card = format!(
        "<div class=\"card\" data-state=\"invalid_oauth_state\">{}</div>",
        outcome(
            "failure",
            X_CIRCLE,
            "This sign-in link isn't valid",
            "This sign-in link isn't valid or was already used. Start again from the app.",
            ""
        )
    );
    let response = (
        StatusCode::BAD_REQUEST,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        document("Connect", None, &card),
    )
        .into_response();
    secured(response, &content_security_policy(&[], None), false, "no-store")
}

/// Our own store or key service failed: nothing about the link is known.
fn unavailable() -> Response {
    let card = format!(
        "<div class=\"card\" data-state=\"unavailable\">{}</div>",
        outcome(
            "neutral",
            ALERT_CIRCLE,
            "Connect couldn't load",
            "The service is unavailable right now. Reload this page in a few minutes.",
            "",
        )
    );
    let response = (
        StatusCode::SERVICE_UNAVAILABLE,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        document("Connect", None, &card),
    )
        .into_response();
    secured(response, &content_security_policy(&[], None), false, "no-store")
}

/// `#RRGGBB` as stored, or the brand's ink: nothing else reaches the style.
fn icon_color(color: &str) -> &str {
    let valid = color.len() == 7 && color.starts_with('#') && color[1..].bytes().all(|byte| byte.is_ascii_hexdigit());
    if valid { color } else { "#10191d" }
}

fn document(title: &str, style: Option<&str>, card: &str) -> String {
    let style = style.map(|style| format!("<style>{style}</style>")).unwrap_or_default();
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"referrer\" content=\"no-referrer\">\n<meta name=\"robots\" content=\"noindex, nofollow\">\n\
         <meta name=\"color-scheme\" content=\"light dark\">\n<title>{title}</title>\n\
         <link rel=\"icon\" href=\"{icon}\" type=\"image/svg+xml\">\n\
         <link rel=\"stylesheet\" href=\"{css_url}\" integrity=\"{css}\">\n{style}\n\
         <script src=\"{js_url}\" integrity=\"{js}\" defer></script>\n</head>\n<body>\n\
         <main class=\"frame\">\n{card}\n<p class=\"secured\">{LOCK}Secured by {MARK}<strong>Zupflow</strong></p>\n\
         </main>\n</body>\n</html>\n",
        title = escape(title),
        icon = *ICON_URL,
        css_url = *STYLESHEET_URL,
        css = *STYLESHEET_INTEGRITY,
        js_url = *SCRIPT_URL,
        js = *SCRIPT_INTEGRITY,
    )
}

// --- States -------------------------------------------------------------------

fn pending(app: &App, session: &ConnectSession, verb: &str, account: Option<&ConnectedAccount>) -> String {
    let name = escape(&app.name);
    let subtitle = match account {
        None => format!("So your assistant can work with your {} account.", app.name),
        Some(_) if matches!(app.auth, AuthTemplate::McpOAuth { identity_url: None, .. }) => format!(
            // Nothing names the account, so the person is the only check that it's the same one.
            "Sign in with the same {} account you connected before. Zupflow may not be able to tell which \
             account it is, so make sure it's the same one.",
            app.name
        ),
        Some(account) => format!(
            "Use {} \u{2014} the account already linked.",
            account.email.as_deref().unwrap_or(&account.display_name)
        ),
    };
    let badge = if session.environment == Environment::Sandbox {
        "<span class=\"env-badge\">Sandbox</span>"
    } else {
        ""
    };
    let body = match &app.auth {
        AuthTemplate::Token {
            inputs,
            instructions,
            docs_url,
            ..
        } => token_form(app, verb, inputs, instructions, docs_url.as_deref()),
        AuthTemplate::OAuth2 { .. } | AuthTemplate::McpOAuth { .. } => oauth_start(app, session),
    };
    format!(
        "<div class=\"card\" data-state=\"pending\">\n<header class=\"intro\">\
         <span class=\"app-icon\" aria-hidden=\"true\">{glyph}</span>\
         <div class=\"intro-text\"><h1>{verb} {name}</h1><p class=\"muted\">{subtitle}</p></div>{badge}</header>\n\
         <div class=\"body\">\n{body}\n\
         <p class=\"form-error\" role=\"alert\" data-form-error hidden></p>\n\
         <button type=\"button\" class=\"button ghost\" data-action=\"cancel\" data-needs-js disabled>\
         <span class=\"spinner\" aria-hidden=\"true\"></span>Cancel</button>\n\
         <noscript><p class=\"form-error\">Connect needs JavaScript. Turn it on, then reload this page.</p></noscript>\n\
         </div>\n</div>",
        glyph = escape(&app.icon.glyph),
        subtitle = escape(&subtitle),
    )
}

/// The link the token form offers for finding the credential: only an
/// `https` address, opened apart from this page.
fn docs_link(docs_url: Option<&str>) -> String {
    let Some(url) = docs_url
        .and_then(|url| url::Url::parse(url).ok())
        .filter(|url| url.scheme() == "https")
    else {
        return String::new();
    };
    format!(
        "<a class=\"link\" href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">Where do I find this? {EXTERNAL}</a>\n",
        escape(url.as_str())
    )
}

fn token_form(app: &App, verb: &str, inputs: &[TokenInput], instructions: &str, docs_url: Option<&str>) -> String {
    let mut form = String::new();
    // Inputs have no `name`: without the script, a submitted form carries no value anywhere.
    form.push_str("<form class=\"stack\" method=\"post\" autocomplete=\"off\" novalidate data-form=\"credentials\">\n");
    if !instructions.trim().is_empty() {
        let _ = writeln!(form, "<p class=\"muted\">{}</p>", escape(instructions));
    }
    for (index, input) in inputs.iter().enumerate() {
        let key = escape(&input.key);
        let placeholder = input
            .placeholder
            .as_deref()
            .map(|text| format!(" placeholder=\"{}\"", escape(text)))
            .unwrap_or_default();
        let help = input
            .help
            .as_deref()
            .filter(|help| !help.trim().is_empty())
            .map(|help| format!("<p class=\"hint\" data-hint-for=\"{key}\">{}</p>", escape(help)))
            .unwrap_or_default();
        let _ = writeln!(
            form,
            "<div class=\"field\"><label for=\"connect-{index}\">{label}</label>\
             <input id=\"connect-{index}\" data-key=\"{key}\" type=\"{kind}\"{placeholder} autocomplete=\"off\" \
             autocapitalize=\"off\" spellcheck=\"false\" aria-invalid=\"false\">\
             <p class=\"field-error\" role=\"alert\" data-error-for=\"{key}\" hidden></p>{help}</div>",
            label = escape(&input.label),
            kind = if input.secret { "password" } else { "text" },
        );
    }
    form.push_str(&docs_link(docs_url));
    let _ = write!(
        form,
        "<button type=\"submit\" class=\"button primary\" data-needs-js disabled>\
         <span class=\"spinner\" aria-hidden=\"true\"></span>{verb}</button>\n\
         <p class=\"fine\">Sent straight to Zupflow and encrypted. {name} is the only service it's used with.</p>\n\
         </form>",
        name = escape(&app.name),
    );
    form
}

/// What a refused start shows while the session is still pending, by the
/// code the API answered, in the same words as a failed session.
const START_REFUSALS: [&str; 2] = ["configuration_not_ready", "app_not_eligible"];

fn oauth_start(app: &App, session: &ConnectSession) -> String {
    let name = escape(&app.name);
    let mut out = String::from("<div class=\"stack\">\n");
    if !session.requested_scopes.is_empty() {
        let _ = write!(
            out,
            "<div class=\"scopes\"><p class=\"scopes-label\">{name} will ask you to allow</p><ul>"
        );
        for scope in &session.requested_scopes {
            let shown = scope.strip_prefix("https://www.googleapis.com/auth/").unwrap_or(scope);
            let _ = write!(out, "<li title=\"{}\">{}</li>", escape(scope), escape(shown));
        }
        out.push_str("</ul></div>\n");
    }
    out.push_str("<div hidden>");
    for code in START_REFUSALS {
        let (_, body) = hosted_failure(app, Some(code));
        let _ = write!(out, "<p data-failure=\"{code}\">{}</p>", escape(&body));
    }
    let (_, body) = hosted_failure(app, None);
    let _ = write!(out, "<p data-failure=\"default\">{}</p>", escape(&body));
    out.push_str("</div>\n");
    let _ = write!(
        out,
        "<button type=\"button\" class=\"button primary\" data-action=\"authorize\" data-needs-js disabled>\
         <span class=\"spinner\" aria-hidden=\"true\"></span><span class=\"glyph\">{EXTERNAL}</span>\
         <span data-label>Continue to {name}</span></button>\n\
         <p class=\"fine\">You'll sign in on {name}. Zupflow never sees your password.</p>\n</div>"
    );
    out
}

fn finished(session: &ConnectSession, shown: Shown, embedding: &Embedding, content: &str) -> String {
    let back = embedding
        .back
        .as_deref()
        .map(|back| format!(" data-return=\"{}\"", escape(back)))
        .unwrap_or_default();
    format!(
        "<div class=\"card\" data-state=\"{state}\" data-status=\"{state}\" data-session=\"{id}\" \
         data-origins=\"{origins}\"{back}>{content}</div>",
        state = shown.status().as_str(),
        id = escape(&session.id.to_string()),
        origins = escape(&embedding.origins.join(" ")),
    )
}

/// Shown by the script, in a popup, when the window can't close itself.
const CLOSE_NOTE: &str = "<p class=\"fine\" data-close-note hidden>You can close this window.</p>";

fn completed(app: &App, going_back: bool) -> String {
    outcome(
        "success",
        CHECK_CIRCLE,
        &format!("{} connected", app.name),
        if going_back {
            "Taking you back\u{2026}"
        } else {
            "You can close this window."
        },
        CLOSE_NOTE,
    )
}

/// A session that ended without connecting: what happened, and the way back.
/// Nothing here starts it again: only the product that sent the person can.
/// In a popup the script swaps the way back for closing the window.
fn ended(heading: &str, body: &str, back: Option<&str>) -> String {
    let back = back
        .map(|back| {
            format!(
                "<a class=\"button outline\" href=\"{}\" data-action=\"back\">{ARROW_LEFT}Go back</a>",
                escape(back)
            )
        })
        .unwrap_or_default();
    let action = format!(
        "{back}<button type=\"button\" class=\"button outline\" data-action=\"close\" hidden>Close window</button>\
         {CLOSE_NOTE}"
    );
    outcome("failure", X_CIRCLE, heading, body, &action)
}

fn outcome(tone: &str, icon: &str, heading: &str, body: &str, action: &str) -> String {
    format!(
        "<div class=\"outcome\"><svg class=\"outcome-icon {tone}\" {SVG}>{icon}</svg><h1>{}</h1>\
         <p class=\"muted\">{}</p>{action}</div>",
        escape(heading),
        escape(body),
    )
}

/// Why a session failed, for the person connecting their account: they
/// aren't the developer, can't change the app's setup, and only need to know
/// whether starting again can help. The Console's `hostedFailure`, word for word.
pub fn hosted_failure(app: &App, failure_code: Option<&str>) -> (String, String) {
    const FROM: &str = "the app that sent you here";
    let name = &app.name;
    let mcp = matches!(app.auth, AuthTemplate::McpOAuth { .. });
    let (heading, body) = match failure_code {
        Some("consent_denied") => (
            "Access wasn't granted".to_owned(),
            format!("Nothing was connected. To connect {name}, start again from {FROM} and allow access."),
        ),
        Some("account_mismatch") => (
            "That's a different account".to_owned(),
            format!(
                "Nothing changed. Start again from {FROM} and sign in with the {name} account that was connected before."
            ),
        ),
        Some("discovery_failed") => (
            format!("{name} can't be connected yet"),
            format!(
                "{name}'s server doesn't offer a sign-in Zupflow can use, so nothing was connected. Starting again \
                 won't help: contact support for {FROM} and tell them {name} couldn't be connected."
            ),
        ),
        Some("identity_off_server") => (
            format!("{name} isn't set up correctly"),
            format!(
                "Zupflow stopped before sending anything to {name}, so nothing was connected. Starting again won't \
                 help: contact support for {FROM} and tell them {name} couldn't be connected."
            ),
        ),
        Some("provider_unavailable") => (
            format!("{name} couldn't be reached"),
            format!("Nothing was connected. This is usually brief: wait a few minutes, then start again from {FROM}."),
        ),
        Some("token_exchange_failed") => (
            format!("{name} didn't finish signing you in"),
            format!(
                "{who} didn't accept the sign-in, so nothing was connected. Start again from {FROM}. If it keeps \
                 happening, contact its support.",
                who = if mcp { format!("{name}'s server") } else { name.clone() }
            ),
        ),
        Some("authorization_failed") => (
            format!("{name} ended the sign-in"),
            format!("{name} stopped the sign-in with an error, so nothing was connected. Start again from {FROM}."),
        ),
        Some("identity_unavailable") => (
            "Couldn't confirm your account".to_owned(),
            format!(
                "{name} didn't say which account signed in, so nothing was connected. Wait a few minutes, then start \
                 again from {FROM}."
            ),
        ),
        Some("configuration_not_ready" | "installation_changed") => (
            "Something changed while you were signing in".to_owned(),
            format!("Nothing was connected. Start again from {FROM}."),
        ),
        Some("app_not_eligible") => (
            format!("{name} isn't available"),
            format!(
                "{name} can't be connected here anymore, so nothing was connected. If you still need it, contact \
                 support for {FROM}."
            ),
        ),
        _ => (
            "Couldn't connect".to_owned(),
            format!("Nothing was connected. Start again from {FROM}."),
        ),
    };
    (heading, body)
}

// --- Icons --------------------------------------------------------------------

/// The attributes every line icon shares (Lucide's geometry).
const SVG: &str = "viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"2\" \
                   stroke-linecap=\"round\" stroke-linejoin=\"round\" aria-hidden=\"true\"";
const CHECK_CIRCLE: &str = "<circle cx=\"12\" cy=\"12\" r=\"10\"/><path d=\"m9 12 2 2 4-4\"/>";
const X_CIRCLE: &str = "<circle cx=\"12\" cy=\"12\" r=\"10\"/><path d=\"m15 9-6 6\"/><path d=\"m9 9 6 6\"/>";
const ALERT_CIRCLE: &str = "<circle cx=\"12\" cy=\"12\" r=\"10\"/><path d=\"M12 8v4\"/><path d=\"M12 16h.01\"/>";
const EXTERNAL: &str = "<svg viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"2\" \
                        stroke-linecap=\"round\" stroke-linejoin=\"round\" aria-hidden=\"true\" width=\"14\" height=\"14\">\
                        <path d=\"M15 3h6v6\"/><path d=\"M10 14 21 3\"/>\
                        <path d=\"M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6\"/></svg>";
const ARROW_LEFT: &str = "<svg viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"2\" \
                          stroke-linecap=\"round\" stroke-linejoin=\"round\" aria-hidden=\"true\">\
                          <path d=\"m12 19-7-7 7-7\"/><path d=\"M19 12H5\"/></svg>";
const LOCK: &str = "<svg viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"2\" \
                    stroke-linecap=\"round\" stroke-linejoin=\"round\" aria-hidden=\"true\">\
                    <rect width=\"18\" height=\"11\" x=\"3\" y=\"11\" rx=\"2\" ry=\"2\"/>\
                    <path d=\"M7 11V7a5 5 0 0 1 10 0v4\"/></svg>";
/// The B1 Precision mark, from the brand kit's outlined paths.
const MARK: &str = "<svg class=\"mark\" viewBox=\"10 10 82 82\" aria-hidden=\"true\">\
                    <path fill=\"#35C8F5\" d=\"M30 12H64L84 32V50H68V39L57 28H37L28 37V59L39 70H50V86H32L12 66V30Z\"/>\
                    <path fill=\"#35C8F5\" d=\"M60 56L90 73L60 90Z\"/></svg>";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_everything_markup_could_hide_in() {
        assert_eq!(
            escape(r#"<script>alert("x")</script> & 'y'"#),
            "&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt; &amp; &#39;y&#39;"
        );
    }

    #[test]
    fn frames_only_from_exact_origins() {
        let allowed = [
            "https://app.example".to_owned(),
            "http://localhost:3000".to_owned(),
            "https://app.example; script-src *".to_owned(),
            "https://app.example/path".to_owned(),
            "javascript:alert(1)".to_owned(),
        ];
        assert_eq!(
            embedding_origins(&allowed, Environment::Sandbox),
            ["https://app.example", "http://localhost:3000"]
        );
        // `http://localhost` is for Sandbox alone, as everywhere else.
        assert_eq!(
            embedding_origins(&allowed, Environment::Production),
            ["https://app.example"]
        );
        assert!(embedding_origins(&["http://app.example".to_owned()], Environment::Sandbox).is_empty());
        assert!(content_security_policy(&[], None).contains("frame-ancestors 'none';"));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn tells_the_return_url_one_session() {
        let url =
            url::Url::parse("https://app.example/back?zupflow_session=cs_old&tab=apps&zupflow_session=x").unwrap();
        assert_eq!(
            with_session(url, "cs_new"),
            "https://app.example/back?tab=apps&zupflow_session=cs_new"
        );
        let url = url::Url::parse("https://app.example/back").unwrap();
        assert_eq!(
            with_session(url, "cs_new"),
            "https://app.example/back?zupflow_session=cs_new"
        );
    }

    #[test]
    fn styles_the_icon_only_with_a_color() {
        assert_eq!(icon_color("#AABBCC"), "#AABBCC");
        assert_eq!(icon_color("red;}body{display:none"), "#10191d");
        assert_eq!(icon_color("#ééé"), "#10191d");
    }
}
