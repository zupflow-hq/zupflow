//! One call to Slack's Web API, and what came of it, in Zupflow's terms.
//!
//! Every call is a form-encoded `POST` to `{base}/{method}` through the
//! [`OutboundHttp`] it's given, never a client of its own: public HTTPS
//! only, no redirects, bounded in time and size, the token injected last
//! as the `Authorization` header and scrubbed from what comes back.
//!
//! What came back is classified here, by status and by Slack's `error`
//! code, into a [`SlackError`] holding one of [`SlackCode`]'s stable codes.
//! Slack's own words (its `error` values beyond the known ones, its
//! `warning`, `response_metadata.messages`) are never passed on or logged.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value};
use zeroize::Zeroizing;
use zupflow_core::outbound::{
    Credential, Limits, Method, OutboundError, OutboundHttp, OutboundRequest, OutboundResponse, Refusal,
};

/// Slack's Web API.
pub const DEFAULT_API_URL: &str = "https://slack.com/api";

/// How long a `Retry-After` may ask for: longer than an hour is read as an hour.
const MAX_RETRY_AFTER_SECS: u64 = 60 * 60;
/// What a rate limit without a `Retry-After` waits: Slack's tier 1 allows about one call a minute.
const DEFAULT_RETRY_AFTER_SECS: u64 = 60;

/// Where Slack's Web API is, and its consent page beside it: Slack's own
/// by default, or another HTTPS base (a stand-in, for a deployment's own
/// end-to-end checks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackApi {
    base: String,
    authorize_url: String,
}

/// A base URL that isn't an HTTPS URL with a host and nothing but a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the Slack API URL must be an https:// URL with a host, like https://slack.com/api")]
pub struct InvalidApiUrl;

impl Default for SlackApi {
    fn default() -> Self {
        Self {
            base: DEFAULT_API_URL.to_owned(),
            authorize_url: "https://slack.com/oauth/v2/authorize".to_owned(),
        }
    }
}

impl SlackApi {
    /// `base` as the Web API's address: `https`, a host, an optional port
    /// and path, and no user, query or fragment. Slack's consent page is
    /// `/oauth/v2/authorize` at the same origin.
    pub fn new(base: &str) -> Result<Self, InvalidApiUrl> {
        let url = url::Url::parse(base.trim()).map_err(|_| InvalidApiUrl)?;
        let plain = url.scheme() == "https"
            && url.host_str().is_some_and(|host| !host.is_empty())
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none();
        if !plain {
            return Err(InvalidApiUrl);
        }
        let origin = url.origin().ascii_serialization();
        let path = url.path().trim_end_matches('/');
        Ok(Self {
            base: format!("{origin}{path}"),
            authorize_url: format!("{origin}/oauth/v2/authorize"),
        })
    }

    /// The address of one Web API method.
    pub fn method_url(&self, method: &str) -> String {
        format!("{}/{method}", self.base)
    }

    /// Slack's consent page, where someone in the team allows an app.
    pub fn authorize_url(&self) -> &str {
        &self.authorize_url
    }
}

/// What a call authenticates with.
pub(crate) enum Auth<'a> {
    /// A token: a configuration token, or an installed app's bot token.
    Bearer(&'a str),
    /// An app's own client ID and secret (`oauth.v2.access`).
    Basic { id: &'a str, secret: &'a str },
    /// Nothing: the call's argument is its credential (`tooling.tokens.rotate`).
    None,
}

/// Why a call didn't do what it was asked, in Zupflow's stable codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SlackError {
    /// Nothing came back: it timed out or the connection failed partway.
    /// A write may have happened all the same.
    #[error("outcome_unknown")]
    NoAnswer,
    /// Slack couldn't be reached, or answered `5xx`. A write answered
    /// `5xx` may have happened all the same.
    #[error("provider_unavailable")]
    Unavailable,
    /// Slack asked to wait this long before calling again (`429` and its
    /// `Retry-After`). Nothing was done.
    #[error("rate_limited")]
    RateLimited { retry_after: Duration },
    /// Slack refused, with this code.
    #[error("{0}")]
    Refused(SlackCode),
    /// An answer that isn't Slack's: refused before sending (not a public
    /// HTTPS address), redirected, too large, or not the shape Slack answers.
    #[error("unreadable_answer")]
    Unreadable,
}

impl SlackError {
    /// The stable code: what a caller records or shows, never Slack's text.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoAnswer => "outcome_unknown",
            Self::Unavailable => "provider_unavailable",
            Self::RateLimited { .. } => "rate_limited",
            Self::Refused(code) => code.as_str(),
            Self::Unreadable => "unreadable_answer",
        }
    }

    /// Whether a write may have happened though it failed: a send that ends
    /// so is settled by looking for it before it's tried again.
    pub fn outcome_unknown(&self) -> bool {
        match self {
            Self::NoAnswer | Self::Unavailable | Self::Unreadable => true,
            Self::Refused(code) => code.transient(),
            Self::RateLimited { .. } => false,
        }
    }

    /// Whether the token was refused: expired, revoked or wrong.
    pub fn token_refused(&self) -> bool {
        matches!(self, Self::Refused(SlackCode::InvalidAuth))
    }
}

macro_rules! slack_codes {
    ($($(#[$doc:meta])* $variant:ident => $code:literal [$($slack:literal),*],)*) => {
        /// Slack's refusals, read into stable codes. Slack's own `error` values
        /// are matched here and nowhere else; anything not listed is `Other`.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum SlackCode {
            $($(#[$doc])* $variant,)*
            /// A refusal not listed here.
            Other,
        }

        impl SlackCode {
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $code,)*
                    Self::Other => "provider_refused",
                }
            }

            /// Slack's `error` value, read.
            pub fn from_slack(error: &str) -> Self {
                match error {
                    $($($slack)|* => Self::$variant,)*
                    _ => Self::Other,
                }
            }
        }

        impl std::fmt::Display for SlackCode {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

slack_codes! {
    /// The token is expired, revoked, wrong or of the wrong kind.
    InvalidAuth => "invalid_auth" [
        "invalid_auth", "not_authed", "token_expired", "token_revoked", "account_inactive",
        "invalid_refresh_token", "not_allowed_token_type", "invalid_token"
    ],
    MissingScope => "missing_scope" ["missing_scope"],
    NoPermission => "no_permission" ["no_permission", "restricted_action", "access_denied", "ekm_access_denied"],
    InvalidManifest => "invalid_manifest" ["invalid_manifest"],
    AppNotFound => "app_not_found" ["invalid_app_id", "app_not_found", "invalid_app"],
    InvalidCode => "invalid_code" ["invalid_code", "code_already_used", "code_expired"],
    BadRedirectUri => "bad_redirect_uri" ["bad_redirect_uri"],
    InvalidClient => "invalid_client" ["invalid_client_id", "bad_client_secret", "invalid_client"],
    ChannelNotFound => "channel_not_found" ["channel_not_found"],
    NotInChannel => "not_in_channel" ["not_in_channel"],
    IsArchived => "is_archived" ["is_archived"],
    MessageNotFound => "message_not_found" ["message_not_found", "thread_not_found"],
    CantUpdate => "cant_update_message" ["cant_update_message", "edit_window_closed"],
    CantDelete => "cant_delete_message" ["cant_delete_message", "compliance_exports_prevent_deletion"],
    TooLong => "too_long" ["msg_too_long", "msg_blocks_too_long"],
    NoText => "no_text" ["no_text"],
    InvalidMessage => "invalid_message" [
        "invalid_blocks", "invalid_blocks_format", "invalid_metadata_format", "invalid_metadata_schema",
        "metadata_too_large", "markdown_text_conflict"
    ],
    AlreadyReacted => "already_reacted" ["already_reacted"],
    NoReaction => "no_reaction" ["no_reaction"],
    InvalidEmoji => "invalid_emoji" ["invalid_name"],
    TooManyReactions => "too_many_reactions" ["too_many_reactions", "too_many_emoji"],
    UserNotFound => "user_not_found" ["user_not_found", "users_not_found", "user_not_visible"],
    CannotDm => "cannot_dm" ["cannot_dm_bot", "user_disabled"],
    InvalidCursor => "invalid_cursor" ["invalid_cursor"],
    TeamAccessNotGranted => "team_access_not_granted" ["team_access_not_granted"],
    /// Slack failed on its side, said in the answer instead of a `5xx`: try
    /// again. A write may have happened all the same.
    Transient => "provider_error" ["internal_error", "fatal_error", "service_unavailable", "request_timeout"],
}

impl SlackCode {
    /// Whether the refusal is Slack failing on its side, not a refusal.
    pub fn transient(self) -> bool {
        self == Self::Transient
    }
}

/// Reads a `Retry-After` of whole seconds; anything else is Slack's tier-1 minute.
fn retry_after(response: &OutboundResponse) -> Duration {
    let seconds = response
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| value.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_RETRY_AFTER_SECS)
        .clamp(1, MAX_RETRY_AFTER_SECS);
    Duration::from_secs(seconds)
}

/// Calls `method` with `form`, authenticated by `auth`: Slack's answer
/// object when it says `ok`.
pub(crate) async fn call(
    http: &dyn OutboundHttp,
    api: &SlackApi,
    method: &'static str,
    auth: Auth<'_>,
    form: &[(&str, &str)],
) -> Result<Map<String, Value>, SlackError> {
    // The form may carry a secret (a refresh token, a code): encoded once,
    // and that one buffer handed to the transport rather than copied, so no
    // copy of it is left behind here.
    let mut body = Zeroizing::new(serde_urlencoded::to_string(form).map_err(|_| SlackError::Unreadable)?);
    let credential = match auth {
        Auth::Bearer(token) => Some(Credential {
            header: "Authorization".to_owned(),
            value: Zeroizing::new(format!("Bearer {token}")),
        }),
        Auth::Basic { id, secret } => {
            let pair = Zeroizing::new(format!("{id}:{secret}"));
            Some(Credential {
                header: "Authorization".to_owned(),
                value: Zeroizing::new(format!("Basic {}", STANDARD.encode(pair.as_bytes()))),
            })
        }
        Auth::None => None,
    };
    let request = OutboundRequest {
        method: Method::Post,
        url: api.method_url(method),
        headers: vec![
            (
                "Content-Type".to_owned(),
                "application/x-www-form-urlencoded".to_owned(),
            ),
            ("Accept".to_owned(), "application/json".to_owned()),
        ],
        body: Some(std::mem::take(&mut *body).into_bytes()),
        credential,
    };
    let response = match http.send(request, Limits::default()).await {
        Ok(response) => response,
        Err(OutboundError::TimedOut | OutboundError::Failed) => return Err(SlackError::NoAnswer),
        Err(OutboundError::Unreachable | OutboundError::Refused(Refusal::Unresolvable)) => {
            return Err(SlackError::Unavailable);
        }
        Err(OutboundError::Refused(_) | OutboundError::Redirected | OutboundError::TooLarge) => {
            tracing::warn!(event = "slack.call_refused", method);
            return Err(SlackError::Unreadable);
        }
    };
    if response.status == 429 {
        return Err(SlackError::RateLimited {
            retry_after: retry_after(&response),
        });
    }
    if response.status >= 500 {
        return Err(SlackError::Unavailable);
    }
    let Ok(Value::Object(answer)) = serde_json::from_slice::<Value>(&response.body) else {
        tracing::warn!(event = "slack.unreadable_answer", method, status = response.status);
        return Err(SlackError::Unreadable);
    };
    match answer.get("ok") {
        Some(Value::Bool(true)) if response.status == 200 => Ok(answer),
        Some(Value::Bool(_)) => {
            let error = answer.get("error").and_then(Value::as_str).unwrap_or_default();
            if error == "ratelimited" || error == "rate_limited" {
                return Err(SlackError::RateLimited {
                    retry_after: retry_after(&response),
                });
            }
            let code = SlackCode::from_slack(error);
            // The stable code only: Slack's own value may name things.
            tracing::info!(event = "slack.refused", method, code = code.as_str());
            Err(SlackError::Refused(code))
        }
        _ => {
            tracing::warn!(event = "slack.unreadable_answer", method, status = response.status);
            Err(SlackError::Unreadable)
        }
    }
}

/// A string field of an answer.
pub(crate) fn text<'a>(object: &'a Map<String, Value>, field: &str) -> Option<&'a str> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

/// An object field of an answer.
pub(crate) fn object<'a>(object: &'a Map<String, Value>, field: &str) -> Option<&'a Map<String, Value>> {
    object.get(field).and_then(Value::as_object)
}

/// A page's `response_metadata.next_cursor`, when there's another page.
pub(crate) fn next_cursor(answer: &Map<String, Value>) -> Option<String> {
    object(answer, "response_metadata")
        .and_then(|metadata| text(metadata, "next_cursor"))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_an_api_url_as_a_base_and_its_consent_page() {
        let api = SlackApi::new("https://slack.example/api/").expect("a base");
        assert_eq!(
            api.method_url("chat.postMessage"),
            "https://slack.example/api/chat.postMessage"
        );
        assert_eq!(api.authorize_url(), "https://slack.example/oauth/v2/authorize");
        assert_eq!(SlackApi::default(), SlackApi::new(DEFAULT_API_URL).expect("Slack's"));
        for refused in [
            "http://slack.com/api",
            "https://user:pass@slack.com/api",
            "https://slack.com/api?x=1",
            "https://slack.com/api#x",
            "slack.com/api",
            "",
        ] {
            assert_eq!(SlackApi::new(refused), Err(InvalidApiUrl), "{refused}");
        }
    }

    #[test]
    fn reads_slacks_errors_into_stable_codes() {
        assert_eq!(SlackCode::from_slack("token_expired"), SlackCode::InvalidAuth);
        assert_eq!(SlackCode::from_slack("invalid_refresh_token"), SlackCode::InvalidAuth);
        assert_eq!(SlackCode::from_slack("channel_not_found").as_str(), "channel_not_found");
        assert_eq!(SlackCode::from_slack("invalid_name").as_str(), "invalid_emoji");
        // Anything Slack says that isn't listed is one code, never its text.
        assert_eq!(
            SlackCode::from_slack("you_sent_secret_xoxb_123").as_str(),
            "provider_refused"
        );
        assert_eq!(
            SlackError::RateLimited {
                retry_after: Duration::from_secs(3)
            }
            .code(),
            "rate_limited"
        );
        assert!(SlackError::NoAnswer.outcome_unknown() && !SlackError::Refused(SlackCode::NoText).outcome_unknown());
        for transient in [
            "internal_error",
            "fatal_error",
            "service_unavailable",
            "request_timeout",
        ] {
            let code = SlackCode::from_slack(transient);
            assert!(
                code.transient() && SlackError::Refused(code).outcome_unknown(),
                "{transient}"
            );
        }
        assert!(!SlackCode::from_slack("user_not_found").transient());
    }
}
