//! [`FakeSlack`]: Slack's Web API, faked in process for tests, behind the
//! same [`OutboundHttp`] seam the real calls go through. No socket is opened
//! and Slack is never reached.
//!
//! It reads each request as Slack would, from its documented shapes (never
//! this crate's code): a form-encoded `POST` to `{api}/{method}`, the token
//! in `Authorization: Bearer …` (an app's client ID and secret in `Basic …`
//! for `oauth.v2.access`), and answers in Slack's own shapes (`ok`, `error`,
//! `429` with `Retry-After`). It is strict where Slack is: configuration
//! tokens only for `apps.manifest.*`, a refresh token works once, a consent
//! code works once and only for its app's client and redirect URL, a
//! revoked bot token is `invalid_auth`. A manifest that names an events
//! `request_url` gets it checked as Slack does, with a `url_verification`
//! signed by the app's signing secret ([`FakeSlack::deliveries`]): on
//! create, before anyone could hold that secret, so a create that names one
//! is refused, as the unanswerable challenge makes Slack refuse it.
//!
//! Every request is kept in its ledger ([`Call`]). [`FakeSlack::fail_next`]
//! queues what the next call of a method gets instead ([`Fault`]), and
//! [`FakeSlack::slow`] makes a method take a while, so calls overlap.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};
use zupflow_core::outbound::{Limits, OutboundError, OutboundHttp, OutboundRequest, OutboundResponse};

use crate::api::SlackApi;

/// Where the fake answers.
pub const FAKE_API_URL: &str = "https://slack.fake.example/api";
/// When the fake's messages are sent: their `ts` count on from here.
pub const ISSUED_AT: i64 = 1_760_000_000;
/// A configuration token's life, as Slack's.
pub const TOKEN_SECONDS: i64 = 12 * 60 * 60;

/// What the next call of a method gets instead of its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// `429`, with a `Retry-After` of so many seconds.
    RateLimited { secs: u32 },
    /// This status, with an empty body.
    Status(u16),
    /// No answer in time, and nothing done.
    TimedOut,
    /// Done, but the answer never arrives.
    LoseAnswer,
    /// `ok: false` with this `error`.
    Error(String),
}

/// One request as it arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub method: String,
    /// The `Authorization` header's value.
    pub authorization: Option<String>,
    pub content_type: Option<String>,
    pub form: BTreeMap<String, String>,
    /// The body exactly as sent.
    pub raw: String,
}

/// A delivery the fake sent an app's events URL, signed as Slack signs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub url: String,
    /// `X-Slack-Request-Timestamp`.
    pub timestamp: String,
    /// `X-Slack-Signature`.
    pub signature: String,
    pub body: String,
}

/// An app the fake made.
#[derive(Debug, Clone, PartialEq)]
pub struct FakeApp {
    pub id: String,
    pub client_id: String,
    pub client_secret: String,
    pub signing_secret: String,
    pub manifest: Value,
    pub deleted: bool,
    /// The team its configuration token belongs to.
    pub team_id: String,
}

/// A bot token the fake issued for an install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeInstall {
    pub token: String,
    pub app_id: String,
    pub team_id: String,
    pub bot_user_id: String,
    pub revoked: bool,
}

/// A message the fake holds.
#[derive(Debug, Clone, PartialEq)]
pub struct FakeMessage {
    pub channel: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub user: String,
    pub text: String,
    pub blocks: Value,
    pub metadata: Option<Value>,
    pub deleted: bool,
    pub reactions: Vec<String>,
}

#[derive(Debug, Clone)]
struct Refresh {
    team_id: String,
    spent: bool,
}

#[derive(Debug, Clone)]
struct Code {
    app_id: String,
    team_id: String,
    team_name: String,
    used: bool,
}

#[derive(Default)]
struct State {
    /// Live configuration tokens, by token, with their team.
    configuration: BTreeMap<String, String>,
    refresh: BTreeMap<String, Refresh>,
    rotations: u32,
    apps: Vec<FakeApp>,
    codes: BTreeMap<String, Code>,
    installs: Vec<FakeInstall>,
    users: BTreeMap<String, Value>,
    messages: Vec<FakeMessage>,
    sequence: u64,
    faults: VecDeque<(String, Fault)>,
    deliveries: Vec<Delivery>,
    slow: BTreeMap<String, Duration>,
    ledger: Vec<Call>,
}

/// Slack's Web API, in process.
#[derive(Default)]
pub struct FakeSlack {
    state: Mutex<State>,
}

/// Seconds since the epoch, now: when a rotated token is issued (Slack's `iat`).
fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
}

/// An answer: a status, its headers and JSON.
type Answer = (u16, Vec<(String, String)>, Value);

fn ok(mut fields: Value) -> Answer {
    fields["ok"] = json!(true);
    (200, Vec::new(), fields)
}

fn error(code: &str) -> Answer {
    (200, Vec::new(), json!({ "ok": false, "error": code }))
}

impl FakeSlack {
    pub fn new() -> Self {
        Self::default()
    }

    /// Where the fake answers, as the calls are told.
    pub fn api() -> SlackApi {
        SlackApi::new(FAKE_API_URL).unwrap_or_default()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A team admin generated this configuration token and refresh token.
    pub fn issue_configuration_token(&self, token: &str, refresh_token: &str, team_id: &str) {
        let mut state = self.state();
        state.configuration.insert(token.to_owned(), team_id.to_owned());
        state.refresh.insert(
            refresh_token.to_owned(),
            Refresh {
                team_id: team_id.to_owned(),
                spent: false,
            },
        );
    }

    /// The configuration token stops working, as when it expires.
    pub fn expire_configuration_token(&self, token: &str) {
        self.state().configuration.remove(token);
    }

    /// Someone in `team_id` clicked Allow on `app_id`'s consent page: the
    /// code Slack sends the browser back with.
    pub fn allow(&self, app_id: &str, team_id: &str, team_name: &str) -> String {
        let mut state = self.state();
        state.sequence += 1;
        let code = format!("code-{}-{}", app_id, state.sequence);
        state.codes.insert(
            code.clone(),
            Code {
                app_id: app_id.to_owned(),
                team_id: team_id.to_owned(),
                team_name: team_name.to_owned(),
                used: false,
            },
        );
        code
    }

    /// `users.info` answers this user.
    pub fn add_user(&self, user: Value) {
        if let Some(id) = user.get("id").and_then(Value::as_str) {
            self.state().users.insert(id.to_owned(), user);
        }
    }

    /// The next call of `method` gets `fault` instead.
    pub fn fail_next(&self, method: &str, fault: Fault) {
        self.state().faults.push_back((method.to_owned(), fault));
    }

    /// Every call of `method` takes `delay` from now on.
    pub fn slow(&self, method: &str, delay: Duration) {
        self.state().slow.insert(method.to_owned(), delay);
    }

    /// Every request, in order.
    pub fn calls(&self) -> Vec<Call> {
        self.state().ledger.clone()
    }

    /// The requests of one method, in order.
    pub fn calls_to(&self, method: &str) -> Vec<Call> {
        self.calls().into_iter().filter(|call| call.method == method).collect()
    }

    pub fn apps(&self) -> Vec<FakeApp> {
        self.state().apps.clone()
    }

    pub fn installs(&self) -> Vec<FakeInstall> {
        self.state().installs.clone()
    }

    pub fn messages(&self) -> Vec<FakeMessage> {
        self.state().messages.clone()
    }

    /// What it sent apps' events URLs, in order.
    pub fn deliveries(&self) -> Vec<Delivery> {
        self.state().deliveries.clone()
    }

    /// How many times a refresh token was spent.
    pub fn rotations(&self) -> u32 {
        self.state().rotations
    }

    fn bearer(call: &Call) -> Option<&str> {
        call.authorization.as_deref()?.strip_prefix("Bearer ")
    }

    fn configuration_team(state: &State, call: &Call) -> Result<String, Answer> {
        match Self::bearer(call) {
            None => Err(error("not_authed")),
            Some(token) if token.starts_with("xoxb-") => Err(error("not_allowed_token_type")),
            Some(token) => state
                .configuration
                .get(token)
                .cloned()
                .ok_or_else(|| error("invalid_auth")),
        }
    }

    fn install<'a>(state: &'a State, call: &Call) -> Result<&'a FakeInstall, Answer> {
        let token = Self::bearer(call).ok_or_else(|| error("not_authed"))?;
        state
            .installs
            .iter()
            .find(|install| install.token == token && !install.revoked)
            .ok_or_else(|| error("invalid_auth"))
    }

    /// A message's `ts`: the time it's posted, as Slack's are, unique by
    /// its sequence.
    fn next_ts(state: &mut State) -> String {
        state.sequence += 1;
        format!("{}.{:06}", now_seconds().max(ISSUED_AT), state.sequence)
    }

    /// The manifest argument, read as Slack checks it: JSON, named, (for a
    /// create or update) with bot scopes, and its event subscriptions, if
    /// any, with an HTTPS events URL and bot events.
    fn manifest(call: &Call, full: bool) -> Result<Value, Answer> {
        let manifest: Value = call
            .form
            .get("manifest")
            .and_then(|text| serde_json::from_str(text).ok())
            .ok_or_else(|| error("invalid_manifest"))?;
        let named = manifest["display_information"]["name"]
            .as_str()
            .is_some_and(|name| !name.is_empty() && name.chars().count() <= 35);
        let complete = !full || manifest["oauth_config"]["scopes"]["bot"].is_array();
        let events = &manifest["settings"]["event_subscriptions"];
        let subscribed = events.is_null()
            || (events["request_url"]
                .as_str()
                .is_some_and(|url| url.starts_with("https://"))
                && events["bot_events"].is_array());
        if named && complete && subscribed {
            Ok(manifest)
        } else {
            Err(error("invalid_manifest"))
        }
    }

    fn answer(&self, call: &Call) -> Answer {
        let mut state = self.state();
        let form = |name: &str| call.form.get(name).cloned().unwrap_or_default();
        let result: Result<Answer, Answer> = (|| match call.method.as_str() {
            "apps.manifest.validate" => {
                Self::configuration_team(&state, call)?;
                Self::manifest(call, false)?;
                Ok(ok(json!({})))
            }
            "tooling.tokens.rotate" => {
                let refresh_token = form("refresh_token");
                let team_id = match state.refresh.get_mut(&refresh_token) {
                    Some(refresh) if !refresh.spent => {
                        refresh.spent = true;
                        refresh.team_id.clone()
                    }
                    _ => return Err(error("invalid_refresh_token")),
                };
                state.rotations += 1;
                let n = state.rotations;
                let token = format!("xoxe.xoxp-1-rotated-{n}");
                let refresh = format!("xoxe-1-refresh-{n}");
                state.configuration.insert(token.clone(), team_id.clone());
                state.refresh.insert(
                    refresh.clone(),
                    Refresh {
                        team_id: team_id.clone(),
                        spent: false,
                    },
                );
                let iat = now_seconds();
                Ok(ok(json!({
                    "token": token, "refresh_token": refresh, "team_id": team_id, "user_id": "UADMIN",
                    "iat": iat, "exp": iat + TOKEN_SECONDS,
                })))
            }
            "apps.manifest.create" => {
                let team_id = Self::configuration_team(&state, call)?;
                let manifest = Self::manifest(call, true)?;
                // Its challenge would be signed with a secret only this answer gives out.
                if !manifest["settings"]["event_subscriptions"].is_null() {
                    return Err(error("invalid_manifest"));
                }
                state.sequence += 1;
                let n = state.sequence;
                let app = FakeApp {
                    id: format!("A0FAKE{n:05}"),
                    client_id: format!("1000.{n}"),
                    client_secret: format!("client-secret-{n}"),
                    signing_secret: format!("signing-secret-{n}"),
                    manifest,
                    deleted: false,
                    team_id,
                };
                let answer = json!({
                    "app_id": app.id,
                    "credentials": {
                        "client_id": app.client_id, "client_secret": app.client_secret,
                        "verification_token": format!("verification-{n}"), "signing_secret": app.signing_secret,
                    },
                    "oauth_authorize_url": format!("https://slack.com/oauth/v2/authorize?client_id={}", app.client_id),
                });
                state.apps.push(app);
                Ok(ok(answer))
            }
            "apps.manifest.update" | "apps.manifest.delete" => {
                let team_id = Self::configuration_team(&state, call)?;
                let manifest = if call.method == "apps.manifest.update" {
                    Some(Self::manifest(call, true)?)
                } else {
                    None
                };
                let app_id = form("app_id");
                let app = state
                    .apps
                    .iter_mut()
                    .find(|app| app.id == app_id && !app.deleted && app.team_id == team_id)
                    .ok_or_else(|| error("invalid_app_id"))?;
                match manifest {
                    Some(manifest) => {
                        let url = manifest["settings"]["event_subscriptions"]["request_url"]
                            .as_str()
                            .map(str::to_owned);
                        let checked = url.filter(|url| {
                            app.manifest["settings"]["event_subscriptions"]["request_url"].as_str() != Some(url)
                        });
                        let secret = app.signing_secret.clone();
                        app.manifest = manifest;
                        // A new events URL is checked, as Slack checks it.
                        if let Some(url) = checked {
                            let body = json!({
                                "token": "fake-verification", "type": "url_verification",
                                "challenge": format!("challenge-{app_id}"),
                            })
                            .to_string();
                            let timestamp = now_seconds().to_string();
                            let signature = crate::events::sign(secret.as_bytes(), &timestamp, body.as_bytes());
                            state.deliveries.push(Delivery {
                                url,
                                timestamp,
                                signature,
                                body,
                            });
                        }
                        Ok(ok(json!({ "app_id": app_id, "permissions_updated": false })))
                    }
                    None => {
                        app.deleted = true;
                        Ok(ok(json!({})))
                    }
                }
            }
            "oauth.v2.access" => {
                let basic = call
                    .authorization
                    .as_deref()
                    .and_then(|value| value.strip_prefix("Basic "))
                    .and_then(|value| STANDARD.decode(value).ok())
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .ok_or_else(|| error("invalid_client_id"))?;
                let (client_id, client_secret) = basic.split_once(':').ok_or_else(|| error("invalid_client_id"))?;
                let app = state
                    .apps
                    .iter()
                    .find(|app| app.client_id == client_id && !app.deleted)
                    .cloned()
                    .ok_or_else(|| error("invalid_client_id"))?;
                if app.client_secret != client_secret {
                    return Err(error("bad_client_secret"));
                }
                let code_text = form("code");
                let code = state.codes.get_mut(&code_text).ok_or_else(|| error("invalid_code"))?;
                if code.app_id != app.id {
                    return Err(error("invalid_code"));
                }
                if code.used {
                    return Err(error("code_already_used"));
                }
                code.used = true;
                let code = code.clone();
                if app.manifest["oauth_config"]["redirect_urls"][0].as_str() != Some(form("redirect_uri").as_str()) {
                    return Err(error("bad_redirect_uri"));
                }
                state.sequence += 1;
                let n = state.sequence;
                let install = FakeInstall {
                    token: format!("xoxb-fake-{n}"),
                    app_id: app.id.clone(),
                    team_id: code.team_id.clone(),
                    bot_user_id: format!("UBOT{n}"),
                    revoked: false,
                };
                let scopes = app.manifest["oauth_config"]["scopes"]["bot"]
                    .as_array()
                    .map(|scopes| scopes.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(","))
                    .unwrap_or_default();
                let answer = json!({
                    "access_token": install.token, "token_type": "bot", "scope": scopes,
                    "bot_user_id": install.bot_user_id, "app_id": app.id,
                    "team": { "id": code.team_id, "name": code.team_name }, "enterprise": null,
                    "is_enterprise_install": false, "authed_user": { "id": "UINSTALLER" },
                });
                state.installs.push(install);
                Ok(ok(answer))
            }
            "auth.test" => {
                let install = Self::install(&state, call)?;
                let handle = state
                    .apps
                    .iter()
                    .find(|app| app.id == install.app_id)
                    .and_then(|app| app.manifest["features"]["bot_user"]["display_name"].as_str())
                    .unwrap_or("app")
                    .to_owned();
                Ok(ok(json!({
                    "url": "https://team.slack.example/", "team": "Team", "user": handle,
                    "team_id": install.team_id, "user_id": install.bot_user_id, "bot_id": format!("B{}", install.bot_user_id),
                })))
            }
            "auth.revoke" => {
                let token = Self::bearer(call).ok_or_else(|| error("not_authed"))?.to_owned();
                let install = state
                    .installs
                    .iter_mut()
                    .find(|install| install.token == token && !install.revoked)
                    .ok_or_else(|| error("invalid_auth"))?;
                install.revoked = true;
                Ok(ok(json!({ "revoked": true })))
            }
            "chat.postMessage" => {
                let user = Self::install(&state, call)?.bot_user_id.clone();
                let channel = form("channel");
                if channel.is_empty() || channel.starts_with('X') {
                    return Err(error("channel_not_found"));
                }
                let blocks: Value = serde_json::from_str(&form("blocks")).unwrap_or(Value::Null);
                if !blocks.is_null() && !blocks.is_array() {
                    return Err(error("invalid_blocks"));
                }
                let metadata = match call.form.get("metadata") {
                    Some(text) => {
                        Some(serde_json::from_str::<Value>(text).map_err(|_| error("invalid_metadata_format"))?)
                    }
                    None => None,
                };
                let ts = Self::next_ts(&mut state);
                let message = FakeMessage {
                    channel: channel.clone(),
                    ts: ts.clone(),
                    thread_ts: call.form.get("thread_ts").cloned(),
                    user,
                    text: form("text"),
                    blocks,
                    metadata,
                    deleted: false,
                    reactions: Vec::new(),
                };
                state.messages.push(message);
                Ok(ok(
                    json!({ "channel": channel, "ts": ts, "message": { "type": "message", "ts": ts } }),
                ))
            }
            "chat.update" | "chat.delete" => {
                let user = Self::install(&state, call)?.bot_user_id.clone();
                let (channel, ts) = (form("channel"), form("ts"));
                let message = state
                    .messages
                    .iter_mut()
                    .find(|message| message.channel == channel && message.ts == ts && !message.deleted)
                    .ok_or_else(|| error("message_not_found"))?;
                if message.user != user {
                    return Err(error(if call.method == "chat.update" {
                        "cant_update_message"
                    } else {
                        "cant_delete_message"
                    }));
                }
                if call.method == "chat.update" {
                    message.text = form("text");
                } else {
                    message.deleted = true;
                }
                Ok(ok(json!({ "channel": channel, "ts": ts })))
            }
            "reactions.add" | "reactions.remove" => {
                Self::install(&state, call)?;
                let (channel, ts, name) = (form("channel"), form("timestamp"), form("name"));
                let message = state
                    .messages
                    .iter_mut()
                    .find(|message| message.channel == channel && message.ts == ts)
                    .ok_or_else(|| error("message_not_found"))?;
                let has = message.reactions.contains(&name);
                match (call.method == "reactions.add", has) {
                    (true, true) => Err(error("already_reacted")),
                    (false, false) => Err(error("no_reaction")),
                    (true, false) => {
                        message.reactions.push(name);
                        Ok(ok(json!({})))
                    }
                    (false, true) => {
                        message.reactions.retain(|reaction| *reaction != name);
                        Ok(ok(json!({})))
                    }
                }
            }
            "conversations.open" => {
                Self::install(&state, call)?;
                Ok(ok(json!({ "channel": { "id": format!("D{}", form("users")) } })))
            }
            "conversations.info" => {
                Self::install(&state, call)?;
                let channel = form("channel");
                Ok(ok(json!({ "channel": {
                    "id": channel, "name": "general", "is_channel": true, "is_private": false,
                    "is_member": true, "is_ext_shared": false, "topic": { "value": "Everything" }, "num_members": 3,
                } })))
            }
            "conversations.replies" | "conversations.history" => {
                Self::install(&state, call)?;
                let channel = form("channel");
                let oldest = form("oldest");
                let thread = call.form.get("ts").cloned();
                let after = |ts: &str| oldest.is_empty() || ts.parse::<f64>().ok() >= oldest.parse::<f64>().ok();
                let mut messages: Vec<Value> = state
                    .messages
                    .iter()
                    .filter(|message| message.channel == channel && !message.deleted && after(&message.ts))
                    .filter(|message| match &thread {
                        Some(thread) => message.ts == *thread || message.thread_ts.as_ref() == Some(thread),
                        None => message.thread_ts.is_none() || message.thread_ts.as_ref() == Some(&message.ts),
                    })
                    .map(|message| {
                        let mut value = json!({
                            "type": "message", "ts": message.ts, "user": message.user, "text": message.text,
                            "bot_id": format!("B{}", message.user),
                        });
                        if let Some(thread_ts) = &message.thread_ts {
                            value["thread_ts"] = json!(thread_ts);
                        }
                        if call.form.get("include_all_metadata").map(String::as_str) == Some("true")
                            && let Some(metadata) = &message.metadata
                        {
                            value["metadata"] = metadata.clone();
                        }
                        value
                    })
                    .collect();
                if thread.is_none() {
                    messages.reverse();
                }
                Ok(ok(
                    json!({ "messages": messages, "has_more": false, "response_metadata": { "next_cursor": "" } }),
                ))
            }
            "users.info" => {
                Self::install(&state, call)?;
                let user = state
                    .users
                    .get(&form("user"))
                    .cloned()
                    .ok_or_else(|| error("user_not_found"))?;
                Ok(ok(json!({ "user": user })))
            }
            "assistant.threads.setStatus" => {
                Self::install(&state, call)?;
                Ok(ok(json!({})))
            }
            _ => Err(error("unknown_method")),
        })();
        match result {
            Ok(answer) | Err(answer) => answer,
        }
    }
}

#[async_trait]
impl OutboundHttp for FakeSlack {
    async fn send(&self, request: OutboundRequest, _: Limits) -> Result<OutboundResponse, OutboundError> {
        let Some(method) = request.url.strip_prefix(&format!("{FAKE_API_URL}/")).map(str::to_owned) else {
            return Err(OutboundError::Unreachable);
        };
        let raw = String::from_utf8_lossy(request.body.as_deref().unwrap_or_default()).into_owned();
        let call = Call {
            method: method.clone(),
            authorization: request
                .credential
                .as_ref()
                .map(|credential| credential.value.to_string()),
            content_type: request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                .map(|(_, value)| value.clone()),
            form: serde_urlencoded::from_str(&raw).unwrap_or_default(),
            raw,
        };
        let (fault, delay) = {
            let mut state = self.state();
            state.ledger.push(call.clone());
            let fault = state
                .faults
                .iter()
                .position(|(faulted, _)| *faulted == method)
                .and_then(|at| state.faults.remove(at))
                .map(|(_, fault)| fault);
            (fault, state.slow.get(&method).copied())
        };
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        let (status, headers, body) = match fault {
            Some(Fault::RateLimited { secs }) => (
                429,
                vec![("Retry-After".to_owned(), secs.to_string())],
                json!({ "ok": false, "error": "ratelimited" }),
            ),
            Some(Fault::Status(status)) => (status, Vec::new(), Value::Object(Map::new())),
            Some(Fault::TimedOut) => return Err(OutboundError::TimedOut),
            Some(Fault::LoseAnswer) => {
                self.answer(&call);
                return Err(OutboundError::TimedOut);
            }
            Some(Fault::Error(code)) => error(&code),
            None => self.answer(&call),
        };
        let mut headers = headers;
        headers.push(("content-type".to_owned(), "application/json; charset=utf-8".to_owned()));
        Ok(OutboundResponse {
            status,
            headers,
            body: body.to_string().into_bytes(),
        })
    }
}
