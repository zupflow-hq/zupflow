//! Taskboard: a fake MCP app with its own OAuth, for end-to-end tests and
//! local click-throughs. Development only: it is never part of a release, it
//! listens on loopback alone, and its name is under the reserved `.example`
//! domain, so it can never stand in for a real host.
//!
//! It is what Zupflow's `mcp_oauth` sign-in (decision 0039) expects of an
//! MCP server, and strict about it:
//!
//! - **Discovery.** `POST /mcp` without a token answers `401` with a
//!   `WWW-Authenticate` naming the protected-resource metadata (RFC 9728),
//!   served at `/.well-known/oauth-protected-resource/mcp` (and the root's).
//!   The authorization server is the app's own origin; its metadata (RFC
//!   8414) is at `/.well-known/oauth-authorization-server`.
//! - **Dynamic client registration** (RFC 7591) at `/register`: public
//!   clients only (`token_endpoint_auth_method` `none`), HTTPS redirect URIs
//!   or HTTP ones on loopback (RFC 8252), no fragments.
//! - **Authorization code with PKCE** (`S256` only) at `/authorize`: an
//!   unknown client or a redirect URI that isn't exactly a registered one
//!   gets an error page, never a redirect; any other problem goes back to
//!   the redirect URI as an OAuth error. `state` is required, `resource`
//!   (RFC 8707) must name the MCP endpoint, and `scope` only
//!   `tasks:read` and `tasks:write`. A consent page approves or denies;
//!   denying answers `access_denied`.
//! - **Tokens** at `/token`: a code works once, within a minute, for its own
//!   client, redirect URI and verifier; a code used twice revokes what it
//!   was exchanged for. Refresh tokens rotate, and a refresh token used again
//!   revokes its whole family. Access tokens are opaque, expire (an hour by
//!   default) and are good only for this MCP endpoint (and `/me`).
//! - **MCP** (Streamable HTTP, JSON answers) at `/mcp`: `initialize` opens a
//!   session (`Mcp-Session-Id`), and every later request must carry it and
//!   come from the same client. Three tools exercise the tool levels of
//!   decision 0040: `list_tasks` (read-only), `create_task` (writes) and
//!   `delete_task` (destructive).
//! - **The account** at `/me`: one fictional person, Robin Park.
//!
//! Everything is in memory, and each registered client has its own board.
//! [`Taskboard`] has hooks for tests: what was called with which token, the
//! token requests, and ways to expire or revoke every token.

mod mcp;
mod oauth;
mod pages;
mod serve;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;
use serde_json::Value;

pub use serve::{Running, StartError, start};

/// The name Taskboard is served under by default.
pub const DEFAULT_HOST: &str = "taskboard.example";

/// The scopes Taskboard knows.
pub const SCOPES: [(&str, &str); 2] = [
    ("tasks:read", "See the tasks on your board"),
    ("tasks:write", "Create and delete tasks on your board"),
];

/// The one account that signs in: fictional.
pub const ACCOUNT_ID: &str = "tb-user-1";
pub const ACCOUNT_NAME: &str = "Robin Park";
pub const ACCOUNT_EMAIL: &str = "robin@taskboard.example";

/// How long a code, and a consent page, may wait.
const CODE_TTL: Duration = Duration::from_secs(60);
const CONSENT_TTL: Duration = Duration::from_secs(10 * 60);

/// How Taskboard is run.
#[derive(Debug, Clone)]
pub struct Options {
    /// The name it's reached at: under `.example`.
    pub host: String,
    /// The loopback port; `0` lets the system pick.
    pub port: u16,
    /// How long an access token lasts.
    pub access_token_ttl: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.to_owned(),
            port: 0,
            access_token_ttl: Duration::from_secs(60 * 60),
        }
    }
}

/// A registered client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RegisteredClient {
    pub client_id: String,
    pub client_name: Option<String>,
    pub redirect_uris: Vec<String>,
}

/// One `tools/call`, as Taskboard received it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub client_id: String,
    /// The access token it came with.
    pub access_token: String,
    pub tool: String,
    pub arguments: Value,
}

/// One request to the token endpoint, and how it was answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenRequest {
    pub grant_type: String,
    pub client_id: Option<String>,
    /// The `resource` named (RFC 8707); Taskboard requires its MCP server.
    pub resource: Option<String>,
    /// `None` when tokens were issued, otherwise the OAuth error code.
    pub error: Option<String>,
}

/// A task on a board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub notes: Option<String>,
    pub status: String,
}

#[derive(Debug)]
struct Client {
    name: Option<String>,
    redirect_uris: Vec<String>,
}

/// An authorization request waiting on the consent page.
#[derive(Debug, Clone)]
struct Consent {
    client_id: String,
    redirect_uri: String,
    state: String,
    code_challenge: String,
    scopes: Vec<String>,
    expires_at: u64,
}

#[derive(Debug)]
struct Code {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    scopes: Vec<String>,
    expires_at: u64,
    used: bool,
    /// The tokens it was exchanged for, revoked together.
    family: String,
}

#[derive(Debug)]
struct AccessToken {
    client_id: String,
    scopes: Vec<String>,
    expires_at: u64,
    family: String,
    revoked: bool,
}

#[derive(Debug)]
struct RefreshToken {
    client_id: String,
    scopes: Vec<String>,
    family: String,
    /// Exchanged for a new one already.
    used: bool,
    revoked: bool,
}

#[derive(Debug, Default)]
struct Board {
    next: u64,
    tasks: Vec<Task>,
}

/// RFC 9207 test responses.
#[derive(Debug, Clone, Copy, Default)]
pub enum ResponseIssuer {
    #[default]
    Correct,
    Missing,
    Foreign,
}

#[derive(Debug, Default)]
struct State {
    response_issuer: ResponseIssuer,
    refresh_delay: Duration,
    refreshes_in_flight: usize,
    unauthorized_requests: usize,
    mcp_status: Option<u16>,
    webhooks: Vec<Value>,
    clients: HashMap<String, Client>,
    /// In the order registered, for listing.
    client_order: Vec<String>,
    consents: HashMap<String, Consent>,
    codes: HashMap<String, Code>,
    access_tokens: HashMap<String, AccessToken>,
    /// Every access token issued, per client, in order.
    issued: HashMap<String, Vec<String>>,
    refresh_tokens: HashMap<String, RefreshToken>,
    /// MCP session ID to its client and the protocol version agreed.
    sessions: HashMap<String, McpSession>,
    boards: HashMap<String, Board>,
    tool_calls: Vec<ToolCall>,
    token_requests: Vec<TokenRequest>,
}

/// An MCP session: the client that opened it, and the protocol version
/// agreed at `initialize`, which every later request must name.
#[derive(Debug)]
pub(crate) struct McpSession {
    pub(crate) client_id: String,
    pub(crate) protocol_version: &'static str,
}

impl State {
    fn revoke_family(&mut self, family: &str) {
        for token in self.access_tokens.values_mut().filter(|token| token.family == family) {
            token.revoked = true;
        }
        for token in self.refresh_tokens.values_mut().filter(|token| token.family == family) {
            token.revoked = true;
        }
    }
}

/// A running Taskboard's state, shared with its handlers. Cloning shares it.
#[derive(Clone)]
pub struct Taskboard {
    inner: Arc<Inner>,
}

struct Inner {
    base_url: String,
    access_token_ttl: Duration,
    state: Mutex<State>,
}

impl Taskboard {
    fn new(base_url: String, access_token_ttl: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                base_url,
                access_token_ttl,
                state: Mutex::new(State::default()),
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // Nothing panics while holding it; if something did, the state is
        // still consistent enough for a fake.
        self.inner.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `https://{host}:{port}`: the issuer, and the origin of every endpoint.
    pub fn base_url(&self) -> &str {
        &self.inner.base_url
    }

    /// The MCP endpoint: the protected resource.
    pub fn mcp_url(&self) -> String {
        format!("{}/mcp", self.inner.base_url)
    }

    /// The account endpoint, for an app's `identity_url`.
    pub fn identity_url(&self) -> String {
        format!("{}/me", self.inner.base_url)
    }

    fn resource_metadata_url(&self) -> String {
        format!("{}/.well-known/oauth-protected-resource/mcp", self.inner.base_url)
    }

    // --- Hooks for tests ---------------------------------------------------

    pub fn set_response_issuer(&self, issuer: ResponseIssuer) {
        self.state().response_issuer = issuer;
    }
    pub fn forget_clients(&self) {
        self.state().clients.clear();
    }
    pub fn set_refresh_delay(&self, delay: Duration) {
        self.state().refresh_delay = delay;
    }
    pub fn refreshes_in_flight(&self) -> usize {
        self.state().refreshes_in_flight
    }
    pub fn unauthorized_requests(&self) -> usize {
        self.state().unauthorized_requests
    }
    pub fn set_mcp_status(&self, status: Option<u16>) {
        self.state().mcp_status = status;
    }
    pub fn webhooks(&self) -> Vec<Value> {
        self.state().webhooks.clone()
    }

    /// Every client registered, in order.
    pub fn registered_clients(&self) -> Vec<RegisteredClient> {
        let state = self.state();
        state
            .client_order
            .iter()
            .filter_map(|id| {
                state.clients.get(id).map(|client| RegisteredClient {
                    client_id: id.clone(),
                    client_name: client.name.clone(),
                    redirect_uris: client.redirect_uris.clone(),
                })
            })
            .collect()
    }

    /// Every `tools/call` received, in order.
    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.state().tool_calls.clone()
    }

    /// Every token request, in order.
    pub fn token_requests(&self) -> Vec<TokenRequest> {
        self.state().token_requests.clone()
    }

    /// Every access token issued to `client_id`, in order, live or not.
    pub fn access_tokens_issued(&self, client_id: &str) -> Vec<String> {
        self.state().issued.get(client_id).cloned().unwrap_or_default()
    }

    /// The tasks on `client_id`'s board, in order.
    pub fn tasks(&self, client_id: &str) -> Vec<Task> {
        self.state()
            .boards
            .get(client_id)
            .map(|board| board.tasks.clone())
            .unwrap_or_default()
    }

    /// Expires every access token issued so far. Refresh tokens still work.
    pub fn expire_access_tokens(&self) {
        let mut state = self.state();
        for token in state.access_tokens.values_mut() {
            token.expires_at = 0;
        }
    }

    /// Revokes every token issued so far, access and refresh alike.
    pub fn revoke_all_tokens(&self) {
        let mut state = self.state();
        for token in state.access_tokens.values_mut() {
            token.revoked = true;
        }
        for token in state.refresh_tokens.values_mut() {
            token.revoked = true;
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

fn later(ttl: Duration) -> u64 {
    now_ms().saturating_add(u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX))
}

/// A fresh, unguessable value: 32 random bytes, base64url, with a prefix
/// saying what it is.
fn random(prefix: &str) -> String {
    let mut bytes = [0u8; 32];
    // Without the OS's randomness nothing here is safe to hand out, and a
    // fake has no better answer than to stop.
    getrandom::fill(&mut bytes).expect("the OS random number generator is available");
    format!("{prefix}{}", URL_SAFE_NO_PAD.encode(bytes))
}
