//! A run's world for the fault, race and operations tests: one
//! organization's Sandbox workspace, a catalog app with `lookup` (allowed)
//! and `send` (needs approval), installations of it, an agent granted both
//! on each, and the fault kit over the server.
//!
//! Include it beside `common` and `faults` with
//! `#[path = "common/run_setup.rs"] mod run_setup;`.

#![allow(dead_code, clippy::unwrap_used)]

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use zupflow_core::ids::{OrganizationId, RunId, WorkspaceId};

use crate::common::{Server, call};
use crate::faults::{Kit, Point, RecordedCall, Worker};

pub const MCP: &str = "https://tools.example/mcp";

/// What `lookup` is described as.
pub const LOOKUP: &str = "Look something up";

pub fn id(value: &Value) -> &str {
    value["id"].as_str().unwrap()
}

/// An organization's Sandbox workspace with installations of one catalog
/// app, each with its own label and token, and an agent granted `lookup`
/// (allowed) and `send` (needs approval) on each, in the order given.
pub struct Setup {
    pub server: Server,
    pub kit: Kit,
    pub org: String,
    pub ws: String,
    /// The catalog app `tools`.
    pub app: String,
    pub agent: String,
    /// By label.
    pub installations: Vec<(String, String)>,
}

pub async fn setup(installations: &[(&str, &str)]) -> Setup {
    setup_on(Server::new().await, installations).await
}

/// [`setup`] on `server`: one reaching the model providers through their
/// adapters, say.
pub async fn setup_on(server: Server, installations: &[(&str, &str)]) -> Setup {
    server.providers.serve_mcp(
        MCP,
        json!([
            { "name": "lookup", "description": LOOKUP, "inputSchema": { "type": "object" }, "annotations": { "readOnlyHint": true } },
            { "name": "send", "description": "Send a message", "inputSchema": { "type": "object" } },
        ]),
    );
    let org = server.organization("alice", "northwind").await;
    let ws = id(&server.workspace("alice", &org, "sandbox", "sandbox").await).to_owned();
    server.default_model("alice", &org, "sandbox").await;
    let app = tools_app(&server, "alice", &org).await;
    let mut connected = Vec::new();
    for (label, token) in installations {
        connected.push((
            (*label).to_owned(),
            connect_as(&server, "alice", &org, &ws, &app, label, token).await,
        ));
    }
    confirm_levels(&server, "alice", &org, &app).await;
    let agent = helper(&server, "alice", &org, &ws).await;
    let kit = Kit::new(&server);
    let setup = Setup {
        server,
        kit,
        org,
        ws,
        app,
        agent,
        installations: connected,
    };
    for (_, installation) in &setup.installations {
        setup.grant(installation).await;
    }
    setup
}

/// The catalog app `tools` in `org`, configured for Sandbox.
async fn tools_app(server: &Server, user: &str, org: &str) -> String {
    let (status, created) = server
        .send(
            call(user, Method::POST, format!("/v1/organizations/{org}/apps")).body(json!({
                "kind": "mcp", "name": "Tools", "key": "tools", "description": "", "auth_type": "token",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let app = id(&created["data"]).to_owned();
    let (status, body) = server
        .send(
            call(user, Method::PATCH, format!("/v1/organizations/{org}/apps/{app}")).body(json!({ "auth": {
                "type": "token",
                "inputs": [{ "key": "api_key", "label": "API key", "secret": true, "help": null, "placeholder": null, "pattern": null }],
                "placement": { "location": "header", "name": "X-Api-Key", "prefix": "" },
                "verify_url": null, "instructions": "", "docs_url": null,
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = server
        .send(
            call(
                user,
                Method::PUT,
                format!("/v1/organizations/{org}/apps/{app}/environments/sandbox/configuration"),
            )
            .body(json!({ "mcp_server_url": MCP })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    app
}

/// Confirms `lookup` allowed and `send` needing approval, once an
/// installation's tools were discovered.
async fn confirm_levels(server: &Server, user: &str, org: &str, app: &str) {
    let (status, body) = server
        .send(
            call(
                user,
                Method::PATCH,
                format!("/v1/organizations/{org}/apps/{app}/tool-levels"),
            )
            .body(json!({
                "levels": [{ "tool": "lookup", "level": "allow" }, { "tool": "send", "level": "approve" }],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// An agent in `ws`, with nothing granted yet.
async fn helper(server: &Server, user: &str, org: &str, ws: &str) -> String {
    let (status, agent) = server
        .send(
            call(user, Method::POST, format!("/v1/workspaces/{ws}/agents"))
                .naming(org)
                .body(json!({ "name": "Helper", "description": "", "instructions": "Be brief." })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{agent}");
    id(&agent["data"]).to_owned()
}

pub async fn connect(server: &Server, org: &str, ws: &str, app: &str, label: &str, token: &str) -> String {
    connect_as(server, "alice", org, ws, app, label, token).await
}

async fn connect_as(server: &Server, user: &str, org: &str, ws: &str, app: &str, label: &str, token: &str) -> String {
    let (status, session) = server
        .send(
            call(user, Method::POST, format!("/v1/workspaces/{ws}/connect-sessions"))
                .naming(org)
                .body(json!({ "app_id": app, "intent": "create", "label": label, "installation_id": null, "return_url": null })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{session}");
    let reference = session["data"]["url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned();
    let (status, done) = server
        .send(
            call(
                "nobody",
                Method::POST,
                format!("/v1/connect/sessions/{reference}/credentials"),
            )
            .body(json!({ "values": { "api_key": token } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    while zupflow_server::discovery::work_once(&server.state).await {}
    id(&done["data"]["installation"]).to_owned()
}

impl Setup {
    /// `call <tool> <arguments>`, naming the first installation's `tool` as
    /// the model is offered it: `tools_<short_id>_<tool>` (decision 0047).
    pub async fn calling(&self, tool: &str, arguments: &str) -> String {
        let installation = &self.installations[0].1;
        let (status, read) = self
            .kit
            .send(
                call(
                    "alice",
                    Method::GET,
                    format!("/v1/workspaces/{}/installations/{installation}", self.ws),
                )
                .naming(&self.org),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{read}");
        format!(
            "call tools_{}_{tool} {arguments}",
            read["data"]["short_id"].as_str().unwrap()
        )
    }

    pub fn scope(&self) -> (OrganizationId, WorkspaceId) {
        (
            OrganizationId::parse(&self.org).unwrap(),
            WorkspaceId::parse(&self.ws).unwrap(),
        )
    }

    pub async fn grant(&self, installation: &str) {
        let (status, grant) = self
            .kit
            .send(
                call(
                    "alice",
                    Method::POST,
                    format!("/v1/workspaces/{}/agents/{}/grants", self.ws, self.agent),
                )
                .naming(&self.org)
                .body(json!({ "installation_id": installation, "tools": ["lookup", "send"], "approval_required": ["send"] })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{grant}");
    }

    /// Removes the agent's grant on `installation` and grants it again, so
    /// it comes last among the agent's grants.
    pub async fn grant_again(&self, installation: &str) {
        let grants = format!("/v1/workspaces/{}/agents/{}/grants", self.ws, self.agent);
        let (status, listed) = self
            .kit
            .send(call("alice", Method::GET, grants.clone()).naming(&self.org))
            .await;
        assert_eq!(status, StatusCode::OK, "{listed}");
        let grant = listed["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|grant| grant["installation_id"] == installation)
            .unwrap();
        let (status, _) = self
            .kit
            .send(call("alice", Method::DELETE, format!("{grants}/{}", id(grant))).naming(&self.org))
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        self.grant(installation).await;
    }

    /// Starts a run as alice; its ID. Nothing works it yet.
    pub async fn start(&self, input: &str) -> RunId {
        let (status, run) = self
            .kit
            .send(
                call(
                    "alice",
                    Method::POST,
                    format!("/v1/workspaces/{}/agents/{}/runs", self.ws, self.agent),
                )
                .naming(&self.org)
                .body(json!({ "input": input })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{run}");
        RunId::parse(id(&run["data"])).unwrap()
    }

    pub async fn get(&self, run: RunId) -> Value {
        let (status, body) = self
            .kit
            .send(call("alice", Method::GET, format!("/v1/workspaces/{}/runs/{run}", self.ws)).naming(&self.org))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["data"].clone()
    }

    pub async fn cancel(&self, run: RunId) {
        let (status, body) = self
            .kit
            .send(
                call(
                    "alice",
                    Method::POST,
                    format!("/v1/workspaces/{}/runs/{run}/cancel", self.ws),
                )
                .naming(&self.org),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["status"], "cancelled");
    }

    pub async fn calls(&self, run: RunId) -> Vec<RecordedCall> {
        let (org, ws) = self.scope();
        self.kit.progress(org, ws, run).await.calls
    }

    /// The credential each `tools/call` the app's server received carried.
    pub fn credentials_received(&self) -> Vec<Option<(String, String)>> {
        self.server
            .providers
            .received_calls()
            .into_iter()
            .map(|received| received.credential)
            .collect()
    }
}

pub fn main_credential() -> Option<(String, String)> {
    Some(("X-Api-Key".to_owned(), "tk_main".to_owned()))
}

impl Setup {
    /// A run asking for `tools` (`lookup` or `send`), one per argument, in
    /// one turn. Nothing works it yet.
    pub async fn run_calling(&self, tools: &[&str], asked: &[Value]) -> RunId {
        let mut input = Vec::new();
        for (tool, arguments) in tools.iter().zip(asked) {
            input.push(self.calling(tool, &arguments.to_string()).await);
        }
        self.start(&input.join("\n")).await
    }

    /// A `POST` to the API, stopped at `point` as [`Worker::Api`]: let go
    /// on, or crashed there (`dies`) and then sent again, as a client that
    /// got no answer would. The answer the client ends with: the first one
    /// when the request is answered without reaching the point (refused
    /// before it writes). `user` is a persona, or an API key's secret.
    pub async fn api_stopped(
        &self,
        point: Point,
        dies: bool,
        user: &str,
        uri: String,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let organization = (!user.starts_with("zf_")).then_some(self.org.as_str());
        let gate = self.kit.faults.arm(Worker::Api, point);
        let request = self
            .kit
            .spawn_send(user, Method::POST, uri.clone(), organization, body.clone());
        let mut request = request;
        let reached = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            tokio::select! {
                biased;
                () = gate.stopped() => None,
                answer = &mut request => Some(answer.unwrap()),
            }
        })
        .await
        .expect("the request stops or answers");
        // Answered without reaching the point: refused before any write.
        if let Some(answer) = reached {
            self.kit.faults.disarm_all();
            return answer;
        }
        if !dies {
            gate.release();
            return request.await.unwrap();
        }
        request.abort();
        let _ = request.await;
        let mut again = call(user, Method::POST, uri);
        if let Some(organization) = organization {
            again = again.naming(organization);
        }
        if let Some(body) = body {
            again = again.body(body);
        }
        self.kit.send(again).await
    }

    /// A new Sandbox API key's secret: what the organization's backend
    /// decides approvals with.
    pub async fn api_key(&self) -> String {
        self.api_key_in("sandbox").await
    }

    /// A new API key's secret for `environment`.
    pub async fn api_key_in(&self, environment: &str) -> String {
        let (status, key) = self
            .server
            .send(
                call(
                    "alice",
                    Method::POST,
                    format!("/v1/organizations/{}/api-keys", self.org),
                )
                .body(json!({ "name": "Backend", "environment": environment })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{key}");
        key["data"]["secret"].as_str().unwrap().to_owned()
    }

    /// `key` decides `call_id`: the API's answer.
    pub async fn decide(&self, key: &str, run: RunId, call_id: &str, decision: &str) -> (StatusCode, Value) {
        self.kit
            .send(
                call(
                    key,
                    Method::POST,
                    format!("/v1/workspaces/{}/runs/{run}/approvals", self.ws),
                )
                .body(json!({ "call_id": call_id, "decision": decision })),
            )
            .await
    }

    /// The run's timeline, as the API lists it.
    pub async fn events(&self, run: RunId) -> Vec<Value> {
        let (status, body) = self
            .kit
            .send(
                call(
                    "alice",
                    Method::GET,
                    format!("/v1/workspaces/{}/runs/{run}/events", self.ws),
                )
                .naming(&self.org),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["data"].as_array().unwrap().clone()
    }

    /// Every way the run broke what the engine owes, whatever stopped,
    /// crashed or raced on its way, read from the app's server's own ledger,
    /// the store and the timeline: empty when it owes nothing.
    ///
    /// - The ledger: each call received at most once, only with its own
    ///   installation's credential, and nothing received the run didn't
    ///   record and authorize.
    /// - Each call settled; one reported done was received exactly once and
    ///   its answer kept; one never received is never reported sent; one
    ///   received has its answer kept (as its outcome, or as late evidence)
    ///   unless the worker that got it died holding it (`answers_kept`), and
    ///   is then settled `unknown`.
    /// - Committed state: one turn per index, at most one answered model
    ///   attempt per turn and none left prepared; the run finished.
    /// - The timeline: gap-free from 1, starting `run.queued`, with exactly
    ///   one ending event, the run's own.
    pub async fn violations(&self, run: RunId, asked: &[Value], answers_kept: bool) -> Vec<String> {
        use zupflow_core::runs::CallState;
        use zupflow_core::runs::state::ModelAttemptState;

        let mut broken = Vec::new();
        let (org, ws) = self.scope();
        let progress = self.kit.progress(org, ws, run).await;
        if !progress.run.status.is_terminal() {
            broken.push(format!("the run never finished: {:?}", progress.run.status));
        }
        let received = self.server.providers.received_calls();
        for request in &received {
            if request.credential != main_credential() {
                broken.push(format!("a request carried {:?}", request.credential));
            }
            if !asked.contains(&request.arguments) {
                broken.push(format!("received {} that no call asked for", request.arguments));
            }
        }
        if !progress.calls.is_empty() && progress.calls.len() != asked.len() {
            broken.push(format!(
                "{} calls recorded, {} asked",
                progress.calls.len(),
                asked.len()
            ));
        }
        for call in &progress.calls {
            let arguments = Value::Object(call.arguments.clone());
            let times = received.iter().filter(|request| request.arguments == arguments).count();
            let kept = crate::faults::kept_answers(call);
            let what = format!("{arguments} ({:?}, received {times}, kept {kept:?})", call.state);
            if times > 1 {
                broken.push(format!("received more than once: {what}"));
            }
            if !call.state.is_settled() {
                broken.push(format!("left open: {what}"));
            }
            if call.state == CallState::Completed && (times != 1 || !kept.iter().any(|answer| answer.contains("done")))
            {
                broken.push(format!("done, so received once with its answer kept: {what}"));
            }
            if times == 0 && matches!(call.state, CallState::Completed | CallState::Failed) {
                broken.push(format!("reported sent, never received: {what}"));
            }
            if times == 1 {
                if call.attempts.is_empty() {
                    broken.push(format!("received without a send claim: {what}"));
                }
                if !matches!(
                    call.state,
                    CallState::Completed | CallState::Failed | CallState::Unknown
                ) {
                    broken.push(format!("received, settled as never sent: {what}"));
                }
                if answers_kept && kept.is_empty() {
                    broken.push(format!("its answer was lost: {what}"));
                }
                if !answers_kept && kept.is_empty() && call.state != CallState::Unknown {
                    broken.push(format!("an answer lost, not settled unknown: {what}"));
                }
            }
        }
        for (index, turn) in progress.turns.iter().enumerate() {
            if usize::try_from(turn.turn).unwrap() != index {
                broken.push(format!("turn {} at index {index}", turn.turn));
            }
        }
        for turn in 0..progress.turns.len() + 1 {
            let answered = progress
                .model_attempts
                .iter()
                .filter(|attempt| {
                    usize::try_from(attempt.turn).unwrap() == turn && attempt.state == ModelAttemptState::Answered
                })
                .count();
            if answered > 1 {
                broken.push(format!("turn {turn} answered {answered} times"));
            }
        }
        if progress.run.status.is_terminal()
            && progress
                .model_attempts
                .iter()
                .any(|attempt| attempt.state == ModelAttemptState::Prepared)
        {
            broken.push("a model attempt left prepared on a finished run".to_owned());
        }
        let events = self.events(run).await;
        for (index, event) in events.iter().enumerate() {
            if event["sequence"].as_u64() != Some(u64::try_from(index).unwrap() + 1) {
                broken.push(format!("event {index} has sequence {}", event["sequence"]));
            }
        }
        if events.first().map(|event| &event["type"]) != Some(&json!("run.queued")) {
            broken.push("the timeline doesn't start run.queued".to_owned());
        }
        let endings: Vec<&Value> = events
            .iter()
            .map(|event| &event["type"])
            .filter(|kind| matches!(kind.as_str(), Some("run.completed" | "run.failed" | "run.cancelled")))
            .collect();
        let own = json!(format!("run.{}", progress.run.status.as_str()));
        if progress.run.status.is_terminal() && endings != [&own] {
            broken.push(format!(
                "ending events {endings:?} for a run {}",
                progress.run.status.as_str()
            ));
        }
        broken
    }
}

/// Another workspace's run world on the same server and kit: its own
/// organization (or the setup's), workspace, installation of `tools` and
/// agent granted both tools.
pub struct Tenant {
    pub user: String,
    pub org: String,
    pub ws: String,
    pub agent: String,
    pub installation: String,
}

impl Tenant {
    pub fn scope(&self) -> (OrganizationId, WorkspaceId) {
        (
            OrganizationId::parse(&self.org).unwrap(),
            WorkspaceId::parse(&self.ws).unwrap(),
        )
    }
}

impl Setup {
    /// A sibling Sandbox workspace in the setup's own organization.
    pub async fn sibling(&self, slug: &str, token: &str) -> Tenant {
        let ws = id(&self.server.workspace("alice", &self.org, slug, "sandbox").await).to_owned();
        self.tenant("alice", self.org.clone(), ws, self.app.clone(), false, token)
            .await
    }

    /// A Production workspace in the setup's own organization, with `tools`
    /// configured for Production. Its Connect session, agent, grant and runs
    /// come from a live API key (the tenant's `user`): the Console doesn't
    /// act there.
    pub async fn production(&self, slug: &str, token: &str) -> Tenant {
        let ws = id(&self.server.workspace("alice", &self.org, slug, "production").await).to_owned();
        let (status, body) = self
            .server
            .send(
                call(
                    "alice",
                    Method::PUT,
                    format!(
                        "/v1/organizations/{}/apps/{}/environments/production/configuration",
                        self.org, self.app
                    ),
                )
                .body(json!({ "mcp_server_url": MCP })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let key = self.api_key_in("production").await;
        self.server.default_model("alice", &self.org, "production").await;
        self.tenant(&key, self.org.clone(), ws, self.app.clone(), false, token)
            .await
    }

    /// A Sandbox workspace in another organization, `user`'s.
    pub async fn other_organization(&self, user: &str, slug: &str, token: &str) -> Tenant {
        let org = self.server.organization(user, slug).await;
        let ws = id(&self.server.workspace(user, &org, "sandbox", "sandbox").await).to_owned();
        let app = tools_app(&self.server, user, &org).await;
        self.server.default_model(user, &org, "sandbox").await;
        self.tenant(user, org, ws, app, true, token).await
    }

    async fn tenant(&self, user: &str, org: String, ws: String, app: String, new_app: bool, token: &str) -> Tenant {
        let installation = connect_as(&self.server, user, &org, &ws, &app, "Main", token).await;
        if new_app {
            confirm_levels(&self.server, user, &org, &app).await;
        }
        let agent = helper(&self.server, user, &org, &ws).await;
        let (status, grant) = self
            .kit
            .send(
                call(user, Method::POST, format!("/v1/workspaces/{ws}/agents/{agent}/grants"))
                    .naming(&org)
                    .body(json!({ "installation_id": installation, "tools": ["lookup", "send"], "approval_required": ["send"] })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{grant}");
        Tenant {
            user: user.to_owned(),
            org,
            ws,
            agent,
            installation,
        }
    }

    /// Starts a run in `tenant` calling `tool` with `arguments` through its
    /// installation; its ID. Nothing works it yet.
    pub async fn start_in(&self, tenant: &Tenant, tool: &str, arguments: &Value) -> RunId {
        let (status, read) = self
            .kit
            .send(
                call(
                    &tenant.user,
                    Method::GET,
                    format!("/v1/workspaces/{}/installations/{}", tenant.ws, tenant.installation),
                )
                .naming(&tenant.org),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{read}");
        let input = format!(
            "call tools_{}_{tool} {arguments}",
            read["data"]["short_id"].as_str().unwrap()
        );
        let (status, run) = self
            .kit
            .send(
                call(
                    &tenant.user,
                    Method::POST,
                    format!("/v1/workspaces/{}/agents/{}/runs", tenant.ws, tenant.agent),
                )
                .naming(&tenant.org)
                .body(json!({ "input": input })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{run}");
        RunId::parse(id(&run["data"])).unwrap()
    }
}

/// The agent's memory: a `notes` type of the organization, its notes, and
/// runs started with or without it.
impl Setup {
    /// A `notes` memory type of the organization whose agents write at
    /// `writes` (`allow`, `approve` or `deny`): its ID.
    pub async fn notes_type(&self, name: &str, writes: &str) -> String {
        let (status, body) = self
            .kit
            .send(
                call("alice", Method::POST, format!("/v1/organizations/{}/memory-types", self.org))
                    .body(json!({ "name": name, "kind": "notes", "settings": {}, "writes": writes })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        id(&body["data"]).to_owned()
    }

    /// Has the agent remember in `memory_type`, its own type.
    pub async fn remember_in(&self, memory_type: &str) {
        let (status, body) = self
            .kit
            .send(
                call(
                    "alice",
                    Method::PATCH,
                    format!("/v1/workspaces/{}/agents/{}", self.ws, self.agent),
                )
                .naming(&self.org)
                .body(json!({ "memory": "type", "memory_type_id": memory_type })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    /// Gives the agent a `notes` type of its own, writing at `writes`: the
    /// type's ID.
    pub async fn with_memory(&self, writes: &str) -> String {
        let memory_type = self.notes_type("Customer notes", writes).await;
        self.remember_in(&memory_type).await;
        memory_type
    }

    /// Sets the type's `writes`.
    pub async fn set_writes(&self, memory_type: &str, writes: &str) {
        let (status, body) = self
            .kit
            .send(
                call(
                    "alice",
                    Method::PATCH,
                    format!("/v1/organizations/{}/memory-types/{memory_type}", self.org),
                )
                .body(json!({ "writes": writes })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    /// Saves `text` for the agent through the API, as alice: the memory.
    pub async fn save_note(&self, text: &str) -> Value {
        let (status, body) = self
            .kit
            .send(
                call(
                    "alice",
                    Method::POST,
                    format!("/v1/workspaces/{}/agents/{}/memories", self.ws, self.agent),
                )
                .naming(&self.org)
                .body(json!({ "text": text })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["data"].clone()
    }

    /// The agent's memories, newest first, as the API lists them.
    pub async fn notes(&self) -> Vec<Value> {
        let (status, body) = self
            .kit
            .send(
                call(
                    "alice",
                    Method::GET,
                    format!("/v1/workspaces/{}/agents/{}/memories", self.ws, self.agent),
                )
                .naming(&self.org),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["data"].as_array().unwrap().clone()
    }

    /// Starts a run as alice with `memory` (`agent` or `off`); its ID.
    /// Nothing works it yet.
    pub async fn start_with_memory(&self, input: &str, memory: &str) -> RunId {
        let (status, run) = self
            .kit
            .send(
                call(
                    "alice",
                    Method::POST,
                    format!("/v1/workspaces/{}/agents/{}/runs", self.ws, self.agent),
                )
                .naming(&self.org)
                .body(json!({ "input": input, "memory": memory })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{run}");
        assert_eq!(run["data"]["memory"], memory, "{run}");
        RunId::parse(id(&run["data"])).unwrap()
    }
}

/// `call <tool> <arguments>` for one of the agent's memory tools, which the
/// model is offered by their own names.
pub fn calling_memory(tool: &str, arguments: &Value) -> String {
    format!("call {tool} {arguments}")
}
