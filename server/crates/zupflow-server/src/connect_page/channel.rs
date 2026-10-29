//! The hosted page for a channel session: linking a team with its app
//! configuration token, or installing an agent's app in it. The same page
//! as an app's, in its words: the same card, script, policy and failure
//! copy's shape, and nothing shown that isn't escaped. A team-linking link
//! that can't be used right now says so instead of asking for tokens.

use std::fmt::Write as _;

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};
use zupflow_core::channels::{ChannelProviderKind, ConnectTarget};
use zupflow_core::connect::ConnectSession;
use zupflow_core::environment::Environment;

use super::{
    ALERT_CIRCLE, ARROW_LEFT, CHECK_CIRCLE, EXTERNAL, Shown, Unrendered, content_security_policy, document, embedding,
    ended, escape, finished, icon_color, outcome, secured,
};
use crate::auth::AppState;

/// Where a person finds a team's configuration tokens.
const SLACK_TOKENS_DOCS: &str = "https://api.slack.com/authentication/config-tokens";

/// What the session is for, with the names it shows.
enum Subject {
    /// Linking a team.
    Team { provider: ChannelProviderKind },
    /// Installing the agent's app in its team.
    App {
        provider: ChannelProviderKind,
        agent: String,
    },
}

/// The agent whose app the session installs, read in the session's own scope.
async fn subject(state: &AppState, session: &ConnectSession) -> Result<Subject, Unrendered> {
    match session.target {
        Some(ConnectTarget::ChannelAppManager { provider, .. }) => Ok(Subject::Team { provider }),
        Some(ConnectTarget::ChannelApp { provider, app, .. }) => {
            let (organization_id, workspace_id) = (session.organization_id, session.workspace_id);
            let app = match state.channels.find_app(organization_id, workspace_id, app).await {
                Ok(Some(app)) => app,
                // A deleted app's link has nothing left to show.
                Ok(None) => return Err(Unrendered::Gone),
                Err(_) => return Err(Unrendered::Unavailable),
            };
            match state
                .agents
                .find_agent(organization_id, workspace_id, app.agent_id)
                .await
            {
                Ok(Some(agent)) => Ok(Subject::App {
                    provider,
                    agent: agent.name,
                }),
                Ok(None) => Err(Unrendered::Gone),
                Err(_) => Err(Unrendered::Unavailable),
            }
        }
        None => Err(Unrendered::Gone),
    }
}

pub(super) async fn render(state: &AppState, session: &ConnectSession, shown: Shown) -> Result<Response, Unrendered> {
    let subject = subject(state, session).await?;
    let embedding = embedding(state, session).await?;
    let (title, card) = match shown {
        Shown::Pending => {
            let unavailable = crate::channels::unavailable_reason(state, session)
                .await
                .map_err(|_| Unrendered::Unavailable)?;
            let title = match &subject {
                Subject::Team { provider } => format!("Link a {} team", provider.name()),
                Subject::App { provider, agent } => format!("Add {agent} to {}", provider.name()),
            };
            match unavailable {
                Some(reason) => unusable(reason, embedding.back.as_deref()),
                None => (title.clone(), pending(&subject, session, &title)),
            }
        }
        Shown::Completed => {
            let heading = match &subject {
                Subject::Team { provider } => format!("{} team linked", provider.name()),
                Subject::App { provider, agent } => format!("{agent} added to {}", provider.name()),
            };
            let body = if embedding.back.is_some() {
                "Taking you back\u{2026}"
            } else {
                "You can close this window."
            };
            let content = outcome("success", CHECK_CIRCLE, &heading, body, super::CLOSE_NOTE);
            (heading, finished(session, shown, &embedding, &content))
        }
        Shown::Failed | Shown::Cancelled => {
            let (heading, body) = if shown == Shown::Cancelled {
                ("Connect was cancelled".to_owned(), "Nothing was connected.".to_owned())
            } else {
                failure(&subject, session.failure_code.as_deref())
            };
            let content = ended(&heading, &body, embedding.back.as_deref());
            (heading, finished(session, shown, &embedding, &content))
        }
    };
    let style = format!(".app-icon{{background-color:{}}}", icon_color("#10191d"));
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

/// A team-linking link that can't be used right now, instead of asking for
/// tokens the person would make for nothing: the organization doesn't allow
/// its provider, or this deployment doesn't reach it. The person is the
/// customer's team admin, who can change neither, so they're told only what
/// to do: ask for a new link. The session stays pending.
fn unusable(reason: &str, back: Option<&str>) -> (String, String) {
    const HEADING: &str = "This link can't be used right now";
    let back = back
        .map(|back| {
            format!(
                "<a class=\"button outline\" href=\"{}\" data-action=\"back\">{ARROW_LEFT}Go back</a>",
                escape(back)
            )
        })
        .unwrap_or_default();
    let content = outcome(
        "neutral",
        ALERT_CIRCLE,
        HEADING,
        "Nothing was linked. Ask the app that sent you here for a new one.",
        &back,
    );
    (
        HEADING.to_owned(),
        format!(
            "<div class=\"card\" data-state=\"unavailable\" data-reason=\"{}\">{content}</div>",
            escape(reason)
        ),
    )
}

fn pending(subject: &Subject, session: &ConnectSession, title: &str) -> String {
    let (subtitle, body) = match subject {
        Subject::Team { provider } => (
            format!(
                "So Zupflow can create your agents' own apps in your {} team.",
                provider.name()
            ),
            token_form(*provider),
        ),
        Subject::App { provider, agent } => (
            format!(
                "So people in your {} team can mention and message {agent}.",
                provider.name()
            ),
            install_start(*provider, agent),
        ),
    };
    let badge = if session.environment == Environment::Sandbox {
        "<span class=\"env-badge\">Sandbox</span>"
    } else {
        ""
    };
    format!(
        "<div class=\"card\" data-state=\"pending\">\n<header class=\"intro\">\
         <span class=\"app-icon\" aria-hidden=\"true\">#</span>\
         <div class=\"intro-text\"><h1>{title}</h1><p class=\"muted\">{subtitle}</p></div>{badge}</header>\n\
         <div class=\"body\">\n{body}\n\
         <p class=\"form-error\" role=\"alert\" data-form-error hidden></p>\n\
         <button type=\"button\" class=\"button ghost\" data-action=\"cancel\" data-needs-js disabled>\
         <span class=\"spinner\" aria-hidden=\"true\"></span>Cancel</button>\n\
         <noscript><p class=\"form-error\">Connect needs JavaScript. Turn it on, then reload this page.</p></noscript>\n\
         </div>\n</div>",
        title = escape(title),
        subtitle = escape(&subtitle),
    )
}

/// The team's two tokens, pasted: the same form an app's token takes, keyed
/// as the API reads them.
fn token_form(provider: ChannelProviderKind) -> String {
    let name = provider.name();
    let mut form = String::new();
    form.push_str("<form class=\"stack\" method=\"post\" autocomplete=\"off\" novalidate data-form=\"credentials\">\n");
    let _ = writeln!(
        form,
        "<p class=\"muted\">{}</p>",
        escape(&format!(
            "In {name}'s app settings (api.{lower}.com/apps), under Your App Configuration Tokens, generate a token \
             for your team. Paste both the access token and its refresh token here.",
            lower = name.to_lowercase(),
        ))
    );
    let inputs = [
        ("configuration_token", "Access token", "xoxe.xoxp-\u{2026}"),
        ("refresh_token", "Refresh token", "xoxe-\u{2026}"),
    ];
    for (index, (key, label, placeholder)) in inputs.into_iter().enumerate() {
        let _ = writeln!(
            form,
            "<div class=\"field\"><label for=\"connect-{index}\">{label}</label>\
             <input id=\"connect-{index}\" data-key=\"{key}\" type=\"password\" placeholder=\"{placeholder}\" \
             autocomplete=\"off\" autocapitalize=\"off\" spellcheck=\"false\" aria-invalid=\"false\">\
             <p class=\"field-error\" role=\"alert\" data-error-for=\"{key}\" hidden></p></div>",
            label = escape(label),
            placeholder = escape(placeholder),
        );
    }
    let _ = write!(
        form,
        "<a class=\"link\" href=\"{SLACK_TOKENS_DOCS}\" target=\"_blank\" rel=\"noopener noreferrer\">\
         Where do I find these? {EXTERNAL}</a>\n\
         <button type=\"submit\" class=\"button primary\" data-needs-js disabled>\
         <span class=\"spinner\" aria-hidden=\"true\"></span>Link team</button>\n\
         <p class=\"fine\">Sent straight to Zupflow and encrypted. {name} is the only service they're used with, \
         and Zupflow keeps them fresh.</p>\n</form>",
        name = escape(name),
    );
    form
}

/// Continuing to the provider's consent page, with the copy for each way a
/// start can be refused.
fn install_start(provider: ChannelProviderKind, agent: &str) -> String {
    let name = escape(provider.name());
    let subject = Subject::App {
        provider,
        agent: agent.to_owned(),
    };
    let mut out = String::from("<div class=\"stack\">\n<div hidden>");
    for code in ["app_not_eligible", "provider_unavailable"] {
        let (_, body) = failure(&subject, Some(code));
        let _ = write!(out, "<p data-failure=\"{code}\">{}</p>", escape(&body));
    }
    let (_, body) = failure(&subject, None);
    let _ = write!(out, "<p data-failure=\"default\">{}</p>", escape(&body));
    out.push_str("</div>\n");
    let _ = write!(
        out,
        "<button type=\"button\" class=\"button primary\" data-action=\"authorize\" data-needs-js disabled>\
         <span class=\"spinner\" aria-hidden=\"true\"></span><span class=\"glyph\">{EXTERNAL}</span>\
         <span data-label>Continue to {name}</span></button>\n\
         <p class=\"fine\">You'll choose your team and allow the app on {name}. Zupflow never sees your \
         password.</p>\n</div>"
    );
    out
}

/// Why a channel session failed, for the person installing: what happened,
/// and whether starting again can help.
fn failure(subject: &Subject, failure_code: Option<&str>) -> (String, String) {
    const FROM: &str = "the app that sent you here";
    let (provider, agent) = match subject {
        Subject::Team { provider } => {
            let name = provider.name();
            return (
                format!("Couldn't link the {name} team"),
                format!("Nothing was linked. Start again from {FROM}."),
            );
        }
        Subject::App { provider, agent } => (provider.name(), agent.as_str()),
    };
    match failure_code {
        Some("consent_denied") => (
            "Access wasn't granted".to_owned(),
            format!("Nothing was added. To add {agent} to {provider}, start again from {FROM} and allow access."),
        ),
        Some("team_mismatch") => (
            format!("That's a different {provider} team"),
            format!(
                "{agent} can only be added to the {provider} team linked for it, so nothing was added. Start again \
                 from {FROM} and choose that team."
            ),
        ),
        Some("provider_unavailable") => (
            format!("{provider} couldn't be reached"),
            format!("Nothing was added. This is usually brief: wait a few minutes, then start again from {FROM}."),
        ),
        Some("token_exchange_failed") => (
            format!("{provider} didn't finish the install"),
            format!(
                "{provider} didn't accept the install, so nothing was added. Start again from {FROM}. If it keeps \
                 happening, contact support for {FROM}."
            ),
        ),
        Some("authorization_failed") => (
            format!("{provider} ended the install"),
            format!("{provider} stopped the install with an error, so nothing was added. Start again from {FROM}."),
        ),
        Some("installation_changed") => (
            "Something changed while you were installing".to_owned(),
            format!("Nothing was added. Start again from {FROM}."),
        ),
        Some("app_not_eligible") => (
            format!("{agent} isn't available"),
            format!(
                "{agent} can't be added to {provider} here anymore, so nothing was added. If you still need it, \
                 contact support for {FROM}."
            ),
        ),
        _ => (
            format!("Couldn't add {agent}"),
            format!("Nothing was added. Start again from {FROM}."),
        ),
    }
}
