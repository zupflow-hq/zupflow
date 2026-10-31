//! Runs through the API and the run engine (milestone 4): the scripted
//! model calls the tools an agent's grants name through the installation's
//! MCP server, every step lands on the run's timeline, a call that needs
//! approval pauses the run until a backend with an API key decides, and a
//! call cut off mid-flight is never sent again.

#![allow(clippy::unwrap_used)]

mod common;

use axum::http::{Method, StatusCode};
use common::{Server, call, code};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use zupflow_core::ids::{AppId, InstallationId, OrganizationId, RunId, ToolCallId, UserId, WorkspaceId};
use zupflow_core::model_store::RouteState;
use zupflow_core::models::TokenUsage;
use zupflow_core::models::{Completion, ModelProvider, ProviderAccess, ProviderError, ToolSpec, Turn};
use zupflow_core::policy::{
    AppToolLevel, LevelPage, PolicyError, PolicySettings, ToolLevel, ToolPolicy, ToolPolicyStore,
};
use zupflow_core::runs::CallState;
use zupflow_core::runs::digest::Sha256Digest;
use zupflow_core::runs::names::{Candidate, build_offer};
use zupflow_core::runs::reducer::{
    Facts, InstallationCheck, Io, Limits, ModelAnswer, ModelResult, Plan, SendCheck, advance,
};
use zupflow_core::runs::state::{CallDecision, CallRecord, CallVerdict, ClaimantId, InvocationId};
use zupflow_core::runs::store::Applied;
use zupflow_core::scope::{Name, Scope};
use zupflow_core::time::Timestamp;
use zupflow_server::run_engine;

const MCP: &str = "https://tools.example/mcp";

fn id(value: &Value) -> &str {
    value["id"].as_str().unwrap()
}

/// An organization with a Sandbox and a Production workspace, each with an
/// installation of a catalog app whose `lookup` the organization allows and
/// whose `send` it requires approval for, and an agent granted both.
struct Setup {
    server: Server,
    org: String,
    sandbox: String,
    production: String,
    app: String,
    installation: String,
    /// The installation's short ID: its tools reach the model as
    /// `tools_<short_id>_<tool>` (decision 0047).
    short_id: String,
    agent: String,
    production_agent: String,
    /// Sandbox's default model.
    sandbox_model: String,
}

async fn connect(server: &Server, who: &str, org: &str, ws: &str, app: &str) -> String {
    let (status, session) = server
        .send(
            call(who, Method::POST, format!("/v1/workspaces/{ws}/connect-sessions"))
                .naming(org)
                .body(json!({ "app_id": app, "intent": "create", "label": "Main", "installation_id": null, "return_url": null })),
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
            .body(json!({ "values": { "api_key": "tk_1234" } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    while zupflow_server::discovery::work_once(&server.state).await {}
    id(&done["data"]["installation"]).to_owned()
}

async fn agent(server: &Server, org: &str, ws: &str, installation: &str) -> String {
    let (status, agent) = server
        .send(
            call("alice", Method::POST, format!("/v1/workspaces/{ws}/agents"))
                .naming(org)
                .body(json!({ "name": "Helper", "description": "", "instructions": "Be brief." })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{agent}");
    let agent = id(&agent["data"]).to_owned();
    let (status, grant) = server
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/workspaces/{ws}/agents/{agent}/grants"),
            )
            .naming(org)
            .body(
                json!({ "installation_id": installation, "tools": ["lookup", "send"], "approval_required": ["send"] }),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{grant}");
    agent
}

async fn setup() -> Setup {
    let server = Server::new().await;
    server.providers.serve_mcp(
        MCP,
        json!([
            { "name": "lookup", "description": "Look something up", "inputSchema": { "type": "object" }, "annotations": { "readOnlyHint": true } },
            { "name": "send", "inputSchema": { "type": "object" } },
        ]),
    );
    let org = server.organization("alice", "northwind").await;
    let sandbox = id(&server.workspace("alice", &org, "sandbox", "sandbox").await).to_owned();
    let production = id(&server.workspace("alice", &org, "production", "production").await).to_owned();
    let sandbox_model = server.default_model("alice", &org, "sandbox").await;
    server.default_model("alice", &org, "production").await;
    let (_, created) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/apps")).body(json!({
                "kind": "mcp", "name": "Tools", "key": "tools", "description": "", "auth_type": "token",
            })),
        )
        .await;
    let app = id(&created["data"]).to_owned();
    let (status, body) = server
        .send(
            call("alice", Method::PATCH, format!("/v1/organizations/{org}/apps/{app}")).body(json!({ "auth": {
                "type": "token",
                "inputs": [{ "key": "api_key", "label": "API key", "secret": true, "help": null, "placeholder": null, "pattern": null }],
                "placement": { "location": "header", "name": "X-Api-Key", "prefix": "" },
                "verify_url": null, "instructions": "", "docs_url": null,
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for environment in ["sandbox", "production"] {
        let (status, _) = server
            .send(
                call(
                    "alice",
                    Method::PUT,
                    format!("/v1/organizations/{org}/apps/{app}/environments/{environment}/configuration"),
                )
                .body(json!({ "mcp_server_url": MCP })),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    let installation = connect(&server, "alice", &org, &sandbox, &app).await;
    let (status, read) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("/v1/workspaces/{sandbox}/installations/{installation}"),
            )
            .naming(&org),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{read}");
    let short_id = read["data"]["short_id"].as_str().unwrap().to_owned();
    // The Console doesn't connect accounts in Production: a key does.
    let production_key = api_key(&server, &org, "production").await;
    let production_installation = connect(&server, &production_key, &org, &production, &app).await;
    levels(&server, &org, &app, "allow", "approve").await;
    let agent = agent(&server, &org, &sandbox, &installation).await;
    let production_agent = agent_in(&server, &org, &production, &production_installation).await;
    Setup {
        server,
        org,
        sandbox,
        production,
        app,
        installation,
        short_id,
        agent,
        production_agent,
        sandbox_model,
    }
}

async fn agent_in(server: &Server, org: &str, ws: &str, installation: &str) -> String {
    agent(server, org, ws, installation).await
}

async fn levels(server: &Server, org: &str, app: &str, lookup: &str, send: &str) {
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::PATCH,
                format!("/v1/organizations/{org}/apps/{app}/tool-levels"),
            )
            .body(json!({
                "levels": [{ "tool": "lookup", "level": lookup }, { "tool": "send", "level": send }],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

async fn api_key(server: &Server, org: &str, environment: &str) -> String {
    let (status, body) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/api-keys"))
                .body(json!({ "name": "Backend", "environment": environment })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["data"]["secret"].as_str().unwrap().to_owned()
}

impl Setup {
    /// The name the model calls `tool` of the Sandbox installation by.
    fn tool(&self, tool: &str) -> String {
        format!("tools_{}_{tool}", self.short_id)
    }

    /// `call <tool> <arguments>`, naming the tool as the model is offered it.
    fn calling(&self, tool: &str, arguments: &str) -> String {
        format!("call {} {arguments}", self.tool(tool))
    }

    async fn start(&self, who: &str, input: &str) -> (StatusCode, Value) {
        self.start_in(who, &self.sandbox, &self.agent, input).await
    }

    async fn start_in(&self, who: &str, ws: &str, agent: &str, input: &str) -> (StatusCode, Value) {
        self.server
            .send(
                call(who, Method::POST, format!("/v1/workspaces/{ws}/agents/{agent}/runs"))
                    .naming(&self.org)
                    .body(json!({ "input": input })),
            )
            .await
    }

    /// Starts a run as alice and works it as far as it goes; its ID.
    async fn run(&self, input: &str) -> String {
        let (status, run) = self.start("alice", input).await;
        assert_eq!(status, StatusCode::CREATED, "{run}");
        self.work().await;
        id(&run["data"]).to_owned()
    }

    async fn work(&self) {
        while run_engine::work_once(&self.server.state).await {}
    }

    async fn get(&self, run: &str) -> Value {
        let (status, body) = self
            .server
            .send(
                call(
                    "alice",
                    Method::GET,
                    format!("/v1/workspaces/{}/runs/{run}", self.sandbox),
                )
                .naming(&self.org),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["data"].clone()
    }

    async fn events(&self, run: &str) -> Vec<Value> {
        let (status, body) = self
            .server
            .send(
                call(
                    "alice",
                    Method::GET,
                    format!("/v1/workspaces/{}/runs/{run}/events", self.sandbox),
                )
                .naming(&self.org),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["data"].as_array().unwrap().clone()
    }

    async fn types(&self, run: &str) -> Vec<String> {
        self.events(run)
            .await
            .iter()
            .map(|event| event["type"].as_str().unwrap().to_owned())
            .collect()
    }

    async fn resolve(&self, who: &str, run: &str, call_id: &str, decision: &str) -> (StatusCode, Value) {
        self.server
            .send(
                call(
                    who,
                    Method::POST,
                    format!("/v1/workspaces/{}/runs/{run}/approvals", self.sandbox),
                )
                .naming(&self.org)
                .body(json!({ "call_id": call_id, "decision": decision })),
            )
            .await
    }

    /// A running run whose model asked for `tool`, decided allowed and
    /// not sent yet, as a worker leaves it between two steps. The worker
    /// stopped there: its claim has lapsed.
    async fn allowed_call(&self, tool: &str) -> (RunId, ToolCallId) {
        self.stopped_worker(tool, false).await
    }

    /// The same, with the call recorded as sent by a worker that stopped
    /// before sending it: its claim and its send claim have lapsed.
    async fn authorized_call(&self, tool: &str) -> (RunId, ToolCallId) {
        self.stopped_worker(tool, true).await
    }

    async fn stopped_worker(&self, tool: &str, authorized: bool) -> (RunId, ToolCallId) {
        let (status, started) = self.start("alice", "hello").await;
        assert_eq!(status, StatusCode::CREATED);
        let run = RunId::parse(id(&started["data"])).unwrap();
        let (org, ws) = self.scope();
        let state = &self.server.state;
        let installation = InstallationId::parse(&self.installation).unwrap();
        let pinned = state
            .tools
            .list_tools(org, ws, installation, None, 100)
            .await
            .unwrap()
            .items
            .into_iter()
            .find(|registered| registered.name == tool)
            .unwrap();
        let definition = Sha256Digest::from_bytes(pinned.pinned.hash);
        let offer = build_offer([Candidate {
            installation_id: installation,
            installation_short_id: self.short_id.clone(),
            installation_label: "Main".to_owned(),
            app_key: "tools".to_owned(),
            app_name: "Tools".to_owned(),
            tool: tool.to_owned(),
            description: String::new(),
            input_schema: json!({ "type": "object" }),
            definition_sha256: definition,
        }])
        .offer;
        let allow = CallDecision {
            verdict: CallVerdict::Allow,
            policy: "configured".to_owned(),
            mode: Some("manual".to_owned()),
            rule: "allowed".to_owned(),
        };
        let claimant = ClaimantId::from_uuid(uuid::Uuid::now_v7());
        // The worker's last step leaves its claims to lapse at once.
        let lapsing = Limits {
            lease_ms: 1,
            send_lease_ms: 1,
            ..Limits::default()
        };
        let facts = |claim: Option<u64>, more: Facts| Facts {
            claimant: Some(claimant),
            claim,
            model_route: Some(RouteState::Usable { key_version: 1 }),
            ..more
        };
        let step = |facts: Facts| async move {
            let snapshot = state.runs.find_snapshot(org, ws, run).await.unwrap().unwrap();
            let Plan::Apply { request, then } = advance(&snapshot, &facts).unwrap() else {
                panic!("a step to take");
            };
            assert!(matches!(
                state.runs.apply(org, ws, &request).await.unwrap(),
                Applied::Committed(_)
            ));
            then
        };
        step(facts(None, Facts::at(Timestamp::now()))).await;
        let claim = Some(1);
        let Io::CallModel(asked) = step(facts(
            claim,
            Facts {
                offer: Some(offer.clone()),
                ..Facts::at(Timestamp::now())
            },
        ))
        .await
        else {
            panic!("the model is asked");
        };
        let call_id = ToolCallId::generate();
        let completion = zupflow_core::models::Completion {
            reply: zupflow_core::models::Reply::ToolCalls {
                text: None,
                calls: vec![zupflow_core::models::ModelToolCall {
                    id: "m-1".to_owned(),
                    name: self.tool(tool),
                    arguments: Map::new(),
                }],
            },
            usage: TokenUsage::default(),
        };
        step(facts(
            claim,
            Facts {
                model: Some(ModelAnswer {
                    turn: asked.turn,
                    attempt: asked.attempt,
                    result: ModelResult::Answered(completion),
                }),
                decisions: vec![(0, allow.clone())],
                call_ids: vec![call_id],
                limits: if authorized { Limits::default() } else { lapsing },
                ..Facts::at(Timestamp::now())
            },
        ))
        .await;
        if authorized {
            let sent = step(facts(
                claim,
                Facts {
                    send_check: Some(SendCheck {
                        call_id,
                        installation: InstallationCheck::Usable,
                        definition_sha256: Some(definition),
                        decision: allow,
                    }),
                    invocation_id: Some(InvocationId::from_uuid(uuid::Uuid::now_v7())),
                    limits: lapsing,
                    ..Facts::at(Timestamp::now())
                },
            ))
            .await;
            assert!(matches!(sent, Io::SendCall(_)), "authorized, and never sent");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        (run, call_id)
    }

    async fn call_of(&self, run: RunId) -> CallRecord {
        let (org, ws) = self.scope();
        let snapshot = self
            .server
            .state
            .runs
            .find_snapshot(org, ws, run)
            .await
            .unwrap()
            .unwrap();
        snapshot.calls[0].clone()
    }

    fn scope(&self) -> (OrganizationId, WorkspaceId) {
        (
            OrganizationId::parse(&self.org).unwrap(),
            WorkspaceId::parse(&self.sandbox).unwrap(),
        )
    }
}

#[tokio::test]
async fn answers_from_the_conversation_alone() {
    let setup = setup().await;
    let (status, run) = setup.start("alice", "  hello there  ").await;
    assert_eq!(status, StatusCode::CREATED, "{run}");
    let run = &run["data"];
    assert_eq!(run["status"], "queued");
    assert_eq!(run["trigger"], "console");
    assert_eq!(run["input"], "hello there");
    assert_eq!(run["agent_name"], "Helper");
    assert!(run["session_id"].as_str().unwrap().starts_with("ses_"));
    setup.work().await;
    let done = setup.get(id(run)).await;
    assert_eq!(done["status"], "completed");
    assert_eq!(done["output"], "You said: hello there");
    assert_eq!(done["error"], Value::Null);
    assert!(done["usage"]["input_tokens"].as_u64().unwrap() > 0);
    assert_eq!(done["usage"]["tool_calls"], 0);
    assert!(done["started_at"].is_string() && done["finished_at"].is_string());
    assert_eq!(
        setup.types(id(run)).await,
        ["run.queued", "run.started", "message.assistant", "run.completed"]
    );
    let events = setup.events(id(run)).await;
    // The run names the provider's model, the organization's model and the
    // connection it went through.
    assert_eq!(events[1]["data"]["model"], common::DEFAULT_MODEL);
    assert_eq!(events[1]["data"]["model_id"], setup.sandbox_model);
    assert!(events[1]["data"]["connection_id"].as_str().unwrap().starts_with("mcn_"));
    assert_eq!(events[3]["data"]["usage"], done["usage"]);
    let sequences: Vec<u64> = events.iter().map(|event| event["sequence"].as_u64().unwrap()).collect();
    assert_eq!(sequences, [1, 2, 3, 4]);
    // `after` reads the rest of the timeline; one that isn't a number matches nothing.
    for (after, count) in [("2", 2), ("2.5", 2), ("-1", 4), ("x", 0), ("", 4)] {
        let (status, body) = setup
            .server
            .send(
                call(
                    "alice",
                    Method::GET,
                    format!("/v1/workspaces/{}/runs/{}/events?after={after}", setup.sandbox, id(run)),
                )
                .naming(&setup.org),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"].as_array().unwrap().len(), count, "after={after}");
    }
    // The agent's view counts it.
    let (_, agent) = setup
        .server
        .send(
            call(
                "alice",
                Method::GET,
                format!("/v1/workspaces/{}/agents/{}", setup.sandbox, setup.agent),
            )
            .naming(&setup.org),
        )
        .await;
    assert_eq!(agent["data"]["runs_last_24h"], 1);
    assert_eq!(agent["data"]["last_run_at"], done["created_at"]);
}

#[tokio::test]
async fn calls_an_allowed_tool_through_the_installation() {
    let setup = setup().await;
    let run = setup.run(&setup.calling("lookup", "{\"q\":\"invoices\"}")).await;
    let done = setup.get(&run).await;
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(done["usage"]["tool_calls"], 1);
    assert!(
        done["output"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{}: ", setup.tool("lookup"))),
        "{done}"
    );
    assert_eq!(
        setup.server.providers.tool_calls(),
        [("lookup".to_owned(), json!({ "q": "invoices" }))]
    );
    // Sent with the installation's credential, as its template places it.
    assert!(
        setup
            .server
            .providers
            .sent()
            .iter()
            .filter(|sent| sent.url == MCP)
            .all(|sent| sent.credential == Some(("X-Api-Key".to_owned(), "tk_1234".to_owned())))
    );
    assert_eq!(
        setup.types(&run).await,
        [
            "run.queued",
            "run.started",
            "message.assistant",
            "tool_call.started",
            "tool_call.completed",
            "message.assistant",
            "run.completed"
        ]
    );
    let events = setup.events(&run).await;
    let started = &events[3]["data"];
    assert!(started["call_id"].as_str().unwrap().starts_with("call_"));
    assert_eq!(started["installation_id"], setup.installation.as_str());
    assert_eq!(started["installation_label"], "Main");
    assert_eq!(started["app_name"], "Tools");
    assert_eq!(started["tool"], "lookup");
    assert_eq!(started["arguments"], json!({ "q": "invoices" }));
    let completed = &events[4]["data"];
    assert_eq!(completed["call_id"], started["call_id"]);
    assert_eq!(completed["result"]["structuredContent"]["tool"], "lookup");
    assert!(completed["duration_ms"].is_number());
}

#[tokio::test]
async fn pauses_for_approval_until_a_backend_decides() {
    let setup = setup().await;
    let key = api_key(&setup.server, &setup.org, "sandbox").await;
    let run = setup.run(&setup.calling("send", "{\"to\":\"ops\"}")).await;
    let waiting = setup.get(&run).await;
    assert_eq!(waiting["status"], "waiting_for_approval");
    assert!(
        setup.server.providers.tool_calls().is_empty(),
        "nothing is sent before the decision"
    );
    let events = setup.events(&run).await;
    let requested = events.last().unwrap();
    assert_eq!(requested["type"], "approval.requested");
    assert_eq!(requested["data"]["tool"], "send");
    assert_eq!(requested["data"]["arguments"], json!({ "to": "ops" }));
    let call_id = requested["data"]["call_id"].as_str().unwrap().to_owned();

    // The Console observes; it never decides.
    let (status, body) = setup.resolve("alice", &run, &call_id, "approved").await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "approval_requires_api_key")
    );
    let (status, body) = setup
        .resolve(&key, &run, &ToolCallId::generate().to_string(), "approved")
        .await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "not_waiting"));
    let (status, body) = setup.resolve(&key, &run, &call_id, "maybe").await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, "validation_failed"));
    assert_eq!(body["error"]["fields"]["decision"], "Choose approved or denied.");

    let (status, body) = setup.resolve(&key, &run, &call_id, "approved").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["status"], "running");
    // Decided once.
    let (status, body) = setup.resolve(&key, &run, &call_id, "denied").await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "not_waiting"));
    setup.work().await;
    let done = setup.get(&run).await;
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(setup.server.providers.tool_calls().len(), 1);
    let events = setup.events(&run).await;
    let resolved = events
        .iter()
        .find(|event| event["type"] == "approval.resolved")
        .unwrap();
    assert_eq!(resolved["data"]["call_id"], call_id.as_str());
    assert_eq!(resolved["data"]["decision"], "approved");
    assert!(resolved["data"]["api_key_id"].as_str().unwrap().starts_with("key_"));
    assert!(events.iter().any(|event| event["type"] == "tool_call.completed"));

    // A denial goes back to the model as the call's result; nothing is sent.
    let denied = setup.run(&setup.calling("send", "{\"to\":\"everyone\"}")).await;
    let call_id = setup.events(&denied).await.last().unwrap()["data"]["call_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, _) = setup.resolve(&key, &denied, &call_id, "denied").await;
    assert_eq!(status, StatusCode::OK);
    setup.work().await;
    let done = setup.get(&denied).await;
    assert_eq!(done["status"], "completed");
    assert!(
        done["output"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{} failed: A person denied this call", setup.tool("send"))),
        "{done}"
    );
    assert_eq!(setup.server.providers.tool_calls().len(), 1);
    assert_eq!(done["usage"]["tool_calls"], 0);
}

#[tokio::test]
async fn records_each_decision_with_its_policy_mode_and_rule() {
    let setup = setup().await;
    let run = setup
        .run(&format!(
            "{}\n{}",
            setup.calling("lookup", "{}"),
            setup.calling("send", "{}")
        ))
        .await;
    let (org, ws) = setup.scope();
    let snapshot = setup
        .server
        .state
        .runs
        .find_snapshot(org, ws, RunId::parse(&run).unwrap())
        .await
        .unwrap()
        .unwrap();
    let decisions: Vec<(&str, CallState, Option<&CallDecision>)> = snapshot
        .calls
        .iter()
        .map(|call| {
            (
                call.target.as_ref().unwrap().tool.as_str(),
                call.state,
                call.decision.as_ref(),
            )
        })
        .collect();
    assert_eq!(decisions[0].0, "lookup");
    assert_eq!(decisions[0].1, CallState::Completed);
    let allowed = decisions[0].2.unwrap();
    assert_eq!(
        (&allowed.verdict, allowed.policy.as_str(), allowed.rule.as_str()),
        (&CallVerdict::Allow, "configured", "allowed")
    );
    assert_eq!(allowed.mode.as_deref(), Some("manual"));
    assert_eq!(decisions[1].1, CallState::AwaitingApproval);
    let waiting = decisions[1].2.unwrap();
    assert!(matches!(waiting.verdict, CallVerdict::RequireApproval { .. }));
}

#[tokio::test]
async fn never_offers_a_tool_the_organization_denies() {
    let setup = setup().await;
    levels(&setup.server, &setup.org, &setup.app, "allow", "deny").await;
    let run = setup.run(&setup.calling("send", "{}")).await;
    let done = setup.get(&run).await;
    assert_eq!(done["status"], "completed");
    assert_eq!(
        done["output"],
        format!("I can't use {}: it isn't one of my tools.", setup.tool("send"))
    );
    assert!(setup.server.providers.tool_calls().is_empty());
}

#[tokio::test]
async fn never_sends_a_call_again_once_its_outcome_is_unknown() {
    let setup = setup().await;
    // The server takes the call and never answers usably.
    *setup.server.providers.tool_status.lock().unwrap() = 502;
    let run = setup.run(&setup.calling("lookup", "{}")).await;
    let done = setup.get(&run).await;
    assert_eq!(done["status"], "completed");
    assert_eq!(setup.server.providers.tool_calls().len(), 1);
    let failed = setup
        .events(&run)
        .await
        .into_iter()
        .find(|event| event["type"] == "tool_call.failed")
        .unwrap();
    assert_eq!(failed["data"]["error"]["code"], "outcome_unknown");
    assert!(
        done["output"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{} failed: ", setup.tool("lookup")))
    );

    // A worker stopped after recording a call as sent: the next one settles
    // it as unknown instead of sending it again.
    *setup.server.providers.tool_status.lock().unwrap() = 200;
    let (run, _) = setup.authorized_call("lookup").await;
    setup.work().await;
    let done = setup.get(&run.to_string()).await;
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(setup.server.providers.tool_calls().len(), 1, "not sent again");
    assert_eq!(setup.call_of(run).await.state, CallState::Unknown);
}

#[tokio::test]
async fn fails_the_run_when_the_installation_needs_reconnecting() {
    let setup = setup().await;
    *setup.server.providers.tool_status.lock().unwrap() = 401;
    let run = setup.run(&setup.calling("lookup", "{}")).await;
    let done = setup.get(&run).await;
    assert_eq!(done["status"], "failed");
    assert_eq!(done["error"]["code"], "installation_reconnect_required");
    assert_eq!(
        done["error"]["message"],
        "Main needs to be reconnected before its tools can run."
    );
    let types = setup.types(&run).await;
    assert_eq!(&types[types.len() - 2..], ["tool_call.failed", "run.failed"]);
}

#[tokio::test]
async fn keeps_the_console_out_of_production() {
    let setup = setup().await;
    let (status, body) = setup
        .start_in("alice", &setup.production, &setup.production_agent, "hello")
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "console_read_only_in_production")
    );
    let key = api_key(&setup.server, &setup.org, "production").await;
    let (status, body) = setup
        .start_in(&key, &setup.production, &setup.production_agent, "hello")
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["data"]["trigger"], "api");
    // Cancelling is allowed there: it acts on nothing outside Zupflow.
    let run = id(&body["data"]).to_owned();
    let (status, body) = setup
        .server
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/workspaces/{}/runs/{run}/cancel", setup.production),
            )
            .naming(&setup.org),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["status"], "cancelled");
    // A Production key can't reach the Sandbox run's workspace.
    let sandbox_run = setup.run("hello").await;
    let (status, _) = setup
        .server
        .send(call(
            &key,
            Method::GET,
            format!("/v1/workspaces/{}/runs/{sandbox_run}", setup.sandbox),
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn checks_roles_inputs_and_agents() {
    let setup = setup().await;
    setup.server.member("alice", &setup.org, "victor", "viewer").await;
    let (status, body) = setup.start("victor", "").await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "forbidden"),
        "role first, then the body"
    );
    let (status, body) = setup.start("alice", "   ").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["fields"]["input"], "Write a message.");
    let (status, body) = setup.start("alice", &"x".repeat(8001)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _) = setup.start_in("alice", &setup.sandbox, "agt_nope", "hello").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = setup
        .server
        .send(
            call(
                "alice",
                Method::PATCH,
                format!("/v1/workspaces/{}/agents/{}", setup.sandbox, setup.agent),
            )
            .naming(&setup.org)
            .body(json!({ "status": "paused" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = setup.start("alice", "hello").await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "agent_paused")
    );
    assert_eq!(body["error"]["message"], "Helper is paused. Resume it to start runs.");
}

#[tokio::test]
async fn cancels_once_and_the_engine_leaves_it_cancelled() {
    let setup = setup().await;
    setup.server.member("alice", &setup.org, "victor", "viewer").await;
    let (_, run) = setup.start("alice", "hello").await;
    let run = id(&run["data"]).to_owned();
    let cancel = |who: &'static str| {
        call(
            who,
            Method::POST,
            format!("/v1/workspaces/{}/runs/{run}/cancel", setup.sandbox),
        )
        .naming(&setup.org)
    };
    let (status, _) = setup.server.send(cancel("victor")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = setup.server.send(cancel("alice")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["status"], "cancelled");
    let (status, body) = setup.server.send(cancel("alice")).await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "run_finished"));
    setup.work().await;
    assert_eq!(setup.get(&run).await["status"], "cancelled");
    let events = setup.events(&run).await;
    assert_eq!(events.last().unwrap()["data"], json!({ "cancelled_by": "alice" }));
}

#[tokio::test]
async fn lists_runs_within_their_workspace_and_organization() {
    let setup = setup().await;
    let first = setup.run("hello").await;
    let second = setup.run(&setup.calling("send", "{}")).await;
    let list = |uri: String| setup.server.send(call("alice", Method::GET, uri).naming(&setup.org));
    let (status, body) = list(format!("/v1/workspaces/{}/runs?limit=1", setup.sandbox)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"][0]["id"], second.as_str(), "newest first");
    assert_eq!(body["meta"]["pagination"]["total"], 2);
    let cursor = body["meta"]["pagination"]["next_cursor"].as_str().unwrap().to_owned();
    let (_, next) = list(format!("/v1/workspaces/{}/runs?limit=1&cursor={cursor}", setup.sandbox)).await;
    assert_eq!(next["data"][0]["id"], first.as_str());
    let (_, waiting) = list(format!(
        "/v1/workspaces/{}/runs?status=waiting_for_approval",
        setup.sandbox
    ))
    .await;
    assert_eq!(waiting["data"].as_array().unwrap().len(), 1);
    let (status, body) = list(format!("/v1/workspaces/{}/runs?status=done", setup.sandbox)).await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, "validation_failed"));
    let (_, none) = list(format!("/v1/workspaces/{}/runs?agent_id=nope", setup.sandbox)).await;
    assert_eq!(none["data"], json!([]));
    let (status, _) = list(format!("/v1/workspaces/{}/runs?cursor=bogus", setup.sandbox)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, body) = list(format!("/v1/organizations/{}/runs", setup.org)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"].as_array().unwrap().len(), 2);
    assert_eq!(body["data"][0]["workspace"]["slug"], "sandbox");
    assert_eq!(body["data"][0]["workspace"]["environment"], "sandbox");
    let (_, production) = list(format!("/v1/organizations/{}/runs?environment=production", setup.org)).await;
    assert_eq!(production["data"], json!([]));
    let key = api_key(&setup.server, &setup.org, "production").await;
    let (status, body) = setup
        .server
        .send(call(
            &key,
            Method::GET,
            format!("/v1/organizations/{}/runs?environment=sandbox", setup.org),
        ))
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "environment_mismatch")
    );
    let (_, own) = setup
        .server
        .send(call(&key, Method::GET, format!("/v1/organizations/{}/runs", setup.org)))
        .await;
    assert_eq!(own["data"], json!([]), "a key sees its own environment only");

    // Another organization's member finds nothing.
    let other = setup.server.organization("mallory", "elsewhere").await;
    let (status, _) = setup
        .server
        .send(
            call(
                "mallory",
                Method::GET,
                format!("/v1/workspaces/{}/runs/{first}", setup.sandbox),
            )
            .naming(&other),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = setup
        .server
        .send(call(
            "mallory",
            Method::GET,
            format!("/v1/organizations/{}/runs", setup.org),
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn announces_each_event_on_the_workspace_and_its_environment() {
    let setup = setup().await;
    let (org, ws) = setup.scope();
    let topic = Name::new(run_engine::EVENTS_TOPIC).unwrap();
    let mut workspace = setup
        .server
        .state
        .events
        .subscribe(&Scope::Workspace(org, ws), &topic)
        .await
        .unwrap();
    let mut environment = setup
        .server
        .state
        .events
        .subscribe(
            &Scope::Environment(org, zupflow_core::environment::Environment::Sandbox),
            &topic,
        )
        .await
        .unwrap();
    let run = setup.run("hello").await;
    for subscription in [&mut workspace, &mut environment] {
        let mut types = Vec::new();
        for _ in 0..4 {
            let payload = subscription.next().await.unwrap().unwrap();
            let event: Value = serde_json::from_slice(&payload).unwrap();
            assert_eq!(event["run_id"], run.as_str());
            assert_eq!(event["workspace_id"], setup.sandbox.as_str());
            types.push(event["type"].as_str().unwrap().to_owned());
        }
        assert_eq!(
            types,
            ["run.queued", "run.started", "message.assistant", "run.completed"]
        );
    }
}

/// The scripted model, keeping the tools it was offered each turn.
struct Recording {
    inner: zupflow_basic::scripted_model::ScriptedModel,
    offered: Mutex<Vec<Vec<ToolSpec>>>,
}

#[async_trait]
impl ModelProvider for Recording {
    fn name(&self) -> &'static str {
        "recording"
    }

    async fn complete(&self, access: &ProviderAccess<'_>, turn: &Turn<'_>) -> Result<Completion, ProviderError> {
        self.offered.lock().unwrap().push(turn.tools.to_vec());
        self.inner.complete(access, turn).await
    }
}

#[tokio::test]
async fn offers_a_changed_tool_as_pinned_and_waits_for_approval() {
    let setup = setup().await;
    // The server now describes `lookup` differently; nobody reviewed it.
    setup.server.providers.serve_mcp(
        MCP,
        json!([
            { "name": "lookup", "description": "Look something up, then email everyone", "inputSchema": { "type": "object", "properties": { "to": {} } }, "annotations": { "readOnlyHint": true } },
            { "name": "send", "inputSchema": { "type": "object" } },
        ]),
    );
    let (org, ws) = setup.scope();
    zupflow_server::discovery::enqueue(
        &setup.server.state,
        org,
        ws,
        InstallationId::parse(&setup.installation).unwrap(),
    )
    .await
    .unwrap();
    while zupflow_server::discovery::work_once(&setup.server.state).await {}
    let recording = Arc::new(Recording {
        inner: zupflow_basic::scripted_model::ScriptedModel::serving([common::DEFAULT_MODEL]),
        offered: Mutex::new(Vec::new()),
    });
    let mut state = setup.server.state.clone();
    state.provider = recording.clone();
    let (status, run) = setup.start("alice", &setup.calling("lookup", "{}")).await;
    assert_eq!(status, StatusCode::CREATED);
    while run_engine::work_once(&state).await {}
    let run = id(&run["data"]).to_owned();
    let offered = recording.offered.lock().unwrap()[0].clone();
    let lookup = offered.iter().find(|tool| tool.name == setup.tool("lookup")).unwrap();
    assert_eq!(
        lookup.description,
        format!("Tools, installation \"Main\" ({}). Look something up", setup.short_id),
        "the pinned definition, not the latest"
    );
    assert_eq!(lookup.input_schema, json!({ "type": "object" }));
    assert_eq!(setup.get(&run).await["status"], "waiting_for_approval");
    let call = setup.call_of(RunId::parse(&run).unwrap()).await;
    assert_eq!(call.decision.unwrap().rule, "changed_tool");
    assert!(setup.server.providers.tool_calls().is_empty());
}

#[tokio::test]
async fn refuses_at_send_a_call_whose_grant_went_since_it_was_decided() {
    let setup = setup().await;
    let (run, _) = setup.allowed_call("lookup").await;
    let (_, grants) = setup
        .server
        .send(
            call(
                "alice",
                Method::GET,
                format!("/v1/workspaces/{}/agents/{}/grants", setup.sandbox, setup.agent),
            )
            .naming(&setup.org),
        )
        .await;
    let grant = grants["data"][0]["id"].as_str().unwrap().to_owned();
    let (status, _) = setup
        .server
        .send(
            call(
                "alice",
                Method::DELETE,
                format!("/v1/workspaces/{}/agents/{}/grants/{grant}", setup.sandbox, setup.agent),
            )
            .naming(&setup.org),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    setup.work().await;
    let call = setup.call_of(run).await;
    assert_eq!(call.state, CallState::Refused);
    assert_eq!(
        call.decision.unwrap().rule,
        "not_granted",
        "the new decision is recorded"
    );
    assert!(setup.server.providers.tool_calls().is_empty(), "never sent");
    let failed = setup
        .events(&run.to_string())
        .await
        .into_iter()
        .find(|event| event["type"] == "tool_call.failed")
        .unwrap();
    assert_eq!(failed["data"]["error"]["code"], "tool_refused");
}

#[tokio::test]
async fn decides_again_at_send_on_the_levels_in_force() {
    let setup = setup().await;
    // Denied since it was decided: refused, never sent.
    let (denied, _) = setup.allowed_call("lookup").await;
    levels(&setup.server, &setup.org, &setup.app, "deny", "approve").await;
    setup.work().await;
    let call = setup.call_of(denied).await;
    assert_eq!(call.state, CallState::Refused);
    assert_eq!(call.decision.unwrap().rule, "organization_denies");
    assert!(setup.server.providers.tool_calls().is_empty());

    // Needs approval since: it waits for a decision instead of going out.
    levels(&setup.server, &setup.org, &setup.app, "approve", "approve").await;
    let (waiting, _) = setup.allowed_call("lookup").await;
    setup.work().await;
    assert_eq!(setup.get(&waiting.to_string()).await["status"], "waiting_for_approval");
    let call = setup.call_of(waiting).await;
    assert_eq!(call.state, CallState::AwaitingApproval);
    assert!(matches!(
        call.decision.unwrap().verdict,
        CallVerdict::RequireApproval { .. }
    ));
    assert!(setup.server.providers.tool_calls().is_empty());
}

/// The policy store, except that levels can't be read.
struct UnreadableLevels(Arc<dyn ToolPolicyStore>);

#[async_trait]
impl ToolPolicyStore for UnreadableLevels {
    async fn load_policy(&self, organization_id: OrganizationId) -> Result<ToolPolicy, PolicyError> {
        self.0.load_policy(organization_id).await
    }

    async fn save_policy(
        &self,
        organization_id: OrganizationId,
        settings: PolicySettings,
        updated_by: Option<UserId>,
        now: Timestamp,
        audit: &zupflow_core::audit::Audit<'_, ToolPolicy>,
    ) -> Result<ToolPolicy, PolicyError> {
        self.0
            .save_policy(organization_id, settings, updated_by, now, audit)
            .await
    }

    async fn list_levels(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        after: Option<&str>,
        limit: u32,
    ) -> Result<LevelPage, PolicyError> {
        self.0.list_levels(organization_id, app_id, after, limit).await
    }

    async fn find_levels(&self, _: OrganizationId, _: AppId, _: &[String]) -> Result<Vec<AppToolLevel>, PolicyError> {
        Err(PolicyError::Unavailable)
    }

    async fn set_levels(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        levels: &[(String, ToolLevel)],
        updated_by: Option<UserId>,
        now: Timestamp,
        audit: &zupflow_core::audit::Audit<'_, Vec<AppToolLevel>>,
    ) -> Result<Vec<AppToolLevel>, PolicyError> {
        self.0
            .set_levels(organization_id, app_id, levels, updated_by, now, audit)
            .await
    }

    async fn suggest_levels(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        listed: &[(String, zupflow_core::apps::ToolEffect)],
        now: Timestamp,
    ) -> Result<u64, PolicyError> {
        self.0.suggest_levels(organization_id, app_id, listed, now).await
    }
}

#[tokio::test]
async fn waits_for_approval_when_the_levels_cant_be_read() {
    let setup = setup().await;
    let mut state = setup.server.state.clone();
    state.policies = Arc::new(UnreadableLevels(setup.server.state.policies.clone()));
    let (status, run) = setup.start("alice", &setup.calling("lookup", "{}")).await;
    assert_eq!(status, StatusCode::CREATED);
    while run_engine::work_once(&state).await {}
    let run = RunId::parse(id(&run["data"])).unwrap();
    assert_eq!(setup.get(&run.to_string()).await["status"], "waiting_for_approval");
    let call = setup.call_of(run).await;
    assert_eq!(call.decision.unwrap().rule, "configuration_unavailable");
    assert!(setup.server.providers.tool_calls().is_empty());
}

#[tokio::test]
async fn fails_a_run_whose_job_died() {
    let setup = setup().await;
    let (status, run) = setup.start("alice", "hello").await;
    assert_eq!(status, StatusCode::CREATED);
    let run = id(&run["data"]).to_owned();
    assert_eq!(run_engine::sweep(&setup.server.state).await, 0, "its job is queued");
    // The job's last attempt ran out: the queue buries it.
    let kind = Name::new(run_engine::RUN_AGENT).unwrap();
    let job = setup
        .server
        .state
        .jobs
        .claim_next(std::slice::from_ref(&kind), std::time::Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    setup
        .server
        .state
        .jobs
        .bury(&job.scope, job.id, &job.lease, &Name::new("lease_expired").unwrap())
        .await
        .unwrap();
    assert_eq!(run_engine::sweep(&setup.server.state).await, 1);
    let failed = setup.get(&run).await;
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["error"]["code"], "run_unavailable");
    assert_eq!(setup.types(&run).await.last().unwrap(), "run.failed");
    assert_eq!(run_engine::sweep(&setup.server.state).await, 0, "once");
}

#[tokio::test]
async fn fails_runs_at_once_without_a_model_provider() {
    let setup = setup().await;
    let mut state = setup.server.state.clone();
    state.provider = Arc::new(zupflow_basic::no_model::NoModelProvider);
    let (status, run) = setup.start("alice", &setup.calling("lookup", "{}")).await;
    assert_eq!(status, StatusCode::CREATED);
    while run_engine::work_once(&state).await {}
    let failed = setup.get(id(&run["data"])).await;
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["error"]["code"], "model_unavailable");
    assert!(setup.server.providers.tool_calls().is_empty());
}

#[tokio::test]
async fn gives_the_model_a_denials_reason_and_keeps_approvals_to_their_workspace() {
    let setup = setup().await;
    let key = api_key(&setup.server, &setup.org, "sandbox").await;
    let run = setup.run(&setup.calling("send", "{}")).await;
    let call_id = setup.events(&run).await.last().unwrap()["data"]["call_id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Through another workspace's path, even of the same organization: not found.
    let (_, other) = setup
        .server
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/organizations/{}/workspaces", setup.org),
            )
            .body(json!({
                "name": "Other", "slug": "other", "environment": "sandbox", "external_id": null,
            })),
        )
        .await;
    let (status, _) = setup
        .server
        .send(
            call(
                &key,
                Method::POST,
                format!("/v1/workspaces/{}/runs/{run}/approvals", id(&other["data"])),
            )
            .body(json!({ "call_id": call_id, "decision": "approved" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let resolve = |reason: String| {
        call(
            &key,
            Method::POST,
            format!("/v1/workspaces/{}/runs/{run}/approvals", setup.sandbox),
        )
        .body(json!({ "call_id": call_id, "decision": "denied", "reason": reason }))
    };
    let (status, body) = setup.server.send(resolve("x".repeat(501))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["fields"]["reason"], "Use 500 characters or fewer.");
    assert_eq!(setup.get(&run).await["status"], "waiting_for_approval");
    let (status, _) = setup.server.send(resolve("  Not this week.  ".to_owned())).await;
    assert_eq!(status, StatusCode::OK);
    setup.work().await;
    let done = setup.get(&run).await;
    assert_eq!(done["status"], "completed");
    assert!(
        done["output"]
            .as_str()
            .unwrap()
            .ends_with("Their reason: Not this week."),
        "{done}"
    );
}

/// The run store, except that reading a run fails from the moment a
/// transition commits until the test lets reads through again: a database
/// that answers the write and then stops answering.
struct ReadsFailAfterCommit {
    inner: Arc<dyn zupflow_core::runs::RunStore>,
    failing: std::sync::atomic::AtomicBool,
}

impl ReadsFailAfterCommit {
    fn reads_fail(&self) -> bool {
        self.failing.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn let_reads_through(&self) {
        self.failing.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait]
impl zupflow_core::runs::RunStore for ReadsFailAfterCommit {
    fn durability(&self) -> zupflow_core::runs::store::Durability {
        self.inner.durability()
    }

    async fn apply(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        request: &zupflow_core::runs::transition::TransitionRequest,
    ) -> Result<Applied, zupflow_core::runs::store::StoreError> {
        let applied = self.inner.apply(organization_id, workspace_id, request).await;
        if matches!(applied, Ok(Applied::Committed(_))) {
            self.failing.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        applied
    }

    async fn find_snapshot(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
    ) -> Result<Option<zupflow_core::runs::state::Snapshot>, zupflow_core::runs::store::StoreError> {
        if self.reads_fail() {
            return Err(zupflow_core::runs::store::StoreError::Unavailable);
        }
        self.inner.find_snapshot(organization_id, workspace_id, run_id).await
    }

    async fn find_run(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
    ) -> Result<Option<zupflow_core::runs::Run>, zupflow_core::runs::store::StoreError> {
        if self.reads_fail() {
            return Err(zupflow_core::runs::store::StoreError::Unavailable);
        }
        self.inner.find_run(organization_id, workspace_id, run_id).await
    }

    async fn list_runs(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        filter: &zupflow_core::runs::RunFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<zupflow_core::runs::Page<zupflow_core::runs::Run>, zupflow_core::runs::store::StoreError> {
        self.inner
            .list_runs(organization_id, workspace_id, filter, cursor, limit)
            .await
    }

    async fn list_organization_runs(
        &self,
        organization_id: OrganizationId,
        filter: &zupflow_core::runs::OrganizationRunFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<zupflow_core::runs::Page<zupflow_core::runs::OrganizationRun>, zupflow_core::runs::store::StoreError>
    {
        self.inner
            .list_organization_runs(organization_id, filter, cursor, limit)
            .await
    }

    async fn list_events(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        after: u64,
    ) -> Result<Vec<zupflow_core::runs::RunEvent>, zupflow_core::runs::store::StoreError> {
        self.inner
            .list_events(organization_id, workspace_id, run_id, after)
            .await
    }

    async fn agent_activity(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        since: Timestamp,
    ) -> Result<Vec<zupflow_core::runs::AgentActivity>, zupflow_core::runs::store::StoreError> {
        self.inner.agent_activity(organization_id, workspace_id, since).await
    }

    async fn usage(
        &self,
        organization_id: OrganizationId,
        environment: zupflow_core::environment::Environment,
        since: Timestamp,
    ) -> Result<Vec<zupflow_core::runs::UsageDay>, zupflow_core::runs::store::StoreError> {
        self.inner.usage(organization_id, environment, since).await
    }

    async fn runs_waiting_on(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        after: Option<RunId>,
        limit: u32,
    ) -> Result<Vec<RunId>, zupflow_core::runs::store::StoreError> {
        self.inner
            .runs_waiting_on(organization_id, workspace_id, installation_id, after, limit)
            .await
    }

    async fn runs_waiting_in_type(
        &self,
        organization_id: OrganizationId,
        memory_type_id: zupflow_core::ids::MemoryTypeId,
        after: Option<RunId>,
        limit: u32,
    ) -> Result<Vec<(WorkspaceId, RunId)>, zupflow_core::runs::store::StoreError> {
        self.inner
            .runs_waiting_in_type(organization_id, memory_type_id, after, limit)
            .await
    }

    async fn abandoned_runs(
        &self,
        limit: u32,
    ) -> Result<Vec<(OrganizationId, WorkspaceId, RunId)>, zupflow_core::runs::store::StoreError> {
        self.inner.abandoned_runs(limit).await
    }
}

/// A start, a cancel and a decision answer with the run as their transition
/// committed it, never with a read after the commit: the database failing
/// to answer once the change committed can't turn it into a `503`, which
/// the idempotency middleware never saves, so a retry with the same key
/// replays the first answer instead of starting a second run or answering
/// `409` for a cancel that worked.
#[tokio::test]
async fn answers_with_what_committed_even_when_reads_fail_after() {
    let setup = setup().await;
    let key = api_key(&setup.server, &setup.org, "sandbox").await;
    let failing = Arc::new(ReadsFailAfterCommit {
        inner: setup.server.state.runs.clone(),
        failing: std::sync::atomic::AtomicBool::new(false),
    });
    let mut state = setup.server.state.clone();
    state.runs = failing.clone();
    let app = zupflow_server::app(state, zupflow_server::RequestGuard::default());
    let send = |who: &str, uri: String, key: &str, body: Option<Value>| {
        let app = app.clone();
        let (who, key, org) = (who.to_owned(), key.to_owned(), setup.org.clone());
        async move {
            let mut request = call(&who, Method::POST, uri).naming(&org).idempotent(&key);
            if let Some(body) = body {
                request = request.body(body);
            }
            common::send_to(&app, request).await
        }
    };

    let start = format!("/v1/workspaces/{}/agents/{}/runs", setup.sandbox, setup.agent);
    let input = setup.calling("send", "{}");
    let (status, started) = send("alice", start.clone(), "start-1", Some(json!({ "input": input }))).await;
    assert_eq!(status, StatusCode::CREATED, "{started}");
    assert_eq!(started["data"]["status"], "queued");
    assert!(failing.reads_fail(), "reads fail once the start committed");
    let (status, again) = send("alice", start, "start-1", Some(json!({ "input": input }))).await;
    assert_eq!((status, &again), (StatusCode::CREATED, &started), "replayed");
    failing.let_reads_through();
    let (org, ws) = setup.scope();
    let listed = setup
        .server
        .state
        .runs
        .list_runs(org, ws, &zupflow_core::runs::RunFilter::default(), None, 10)
        .await
        .unwrap();
    assert_eq!(listed.total, 1, "one run, however often it was asked for");

    // Parked on `send`, then decided: the decision answers as it committed.
    let run = id(&started["data"]).to_owned();
    setup.work().await;
    let call_id = setup.events(&run).await.last().unwrap()["data"]["call_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let approvals = format!("/v1/workspaces/{}/runs/{run}/approvals", setup.sandbox);
    let decision = json!({ "call_id": call_id, "decision": "denied" });
    let (status, decided) = send(&key, approvals.clone(), "decide-1", Some(decision.clone())).await;
    assert_eq!(status, StatusCode::OK, "{decided}");
    assert_eq!(decided["data"]["status"], "running");
    assert!(failing.reads_fail());
    let (status, again) = send(&key, approvals, "decide-1", Some(decision)).await;
    assert_eq!((status, &again), (StatusCode::OK, &decided), "replayed, not 409");
    failing.let_reads_through();

    // And a cancel.
    let cancel = format!("/v1/workspaces/{}/runs/{run}/cancel", setup.sandbox);
    let (status, cancelled) = send("alice", cancel.clone(), "cancel-1", None).await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["data"]["status"], "cancelled");
    assert!(cancelled["data"]["finished_at"].is_string());
    assert!(failing.reads_fail());
    let (status, again) = send("alice", cancel, "cancel-1", None).await;
    assert_eq!(
        (status, &again),
        (StatusCode::OK, &cancelled),
        "replayed, not run_finished"
    );
    failing.let_reads_through();
    assert_eq!(
        setup.get(&run).await,
        cancelled["data"],
        "what it answered is what the store holds"
    );
}

impl Setup {
    /// A run parked on `send`, waiting for approval; its ID and the call's.
    async fn waiting_on_send(&self) -> (String, String) {
        let run = self.run(&self.calling("send", "{}")).await;
        assert_eq!(self.get(&run).await["status"], "waiting_for_approval");
        let call_id = self.events(&run).await.last().unwrap()["data"]["call_id"]
            .as_str()
            .unwrap()
            .to_owned();
        (run, call_id)
    }

    /// Approves the call with an API key and works the run to its end: the
    /// code the call failed with, from its timeline.
    async fn approve_and_finish(&self, key: &str, run: &str, call_id: &str) -> String {
        let (status, body) = self.resolve(key, run, call_id, "approved").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        self.work().await;
        let done = self.get(run).await;
        assert_eq!(done["status"], "completed", "the run goes on: {done}");
        assert!(
            done["output"]
                .as_str()
                .unwrap()
                .starts_with(&format!("{} failed: ", self.tool("send"))),
            "the model read why: {done}"
        );
        let events = self.events(run).await;
        assert!(
            !events.iter().any(|event| event["type"] == "tool_call.started"),
            "never recorded as sent"
        );
        let failed = events
            .iter()
            .find(|event| event["type"] == "tool_call.failed" && event["data"]["call_id"] == call_id)
            .unwrap();
        assert_eq!(done["usage"]["tool_calls"], 0);
        failed["data"]["error"]["code"].as_str().unwrap().to_owned()
    }
}

#[tokio::test]
async fn refuses_an_approved_call_whose_installation_was_deleted() {
    let setup = setup().await;
    let key = api_key(&setup.server, &setup.org, "sandbox").await;
    let (run, call_id) = setup.waiting_on_send().await;
    let (status, _) = setup
        .server
        .send(
            call(
                "alice",
                Method::DELETE,
                format!("/v1/workspaces/{}/installations/{}", setup.sandbox, setup.installation),
            )
            .naming(&setup.org),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        setup.approve_and_finish(&key, &run, &call_id).await,
        "installation_removed"
    );
    assert!(setup.server.providers.tool_calls().is_empty(), "nothing sent");
}

#[tokio::test]
async fn refuses_an_approved_call_whose_installation_was_disconnected() {
    let setup = setup().await;
    let key = api_key(&setup.server, &setup.org, "sandbox").await;
    let (run, call_id) = setup.waiting_on_send().await;
    let (status, body) = setup
        .server
        .send(
            call(
                "alice",
                Method::POST,
                format!(
                    "/v1/workspaces/{}/installations/{}/disconnect",
                    setup.sandbox, setup.installation
                ),
            )
            .naming(&setup.org),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        setup.approve_and_finish(&key, &run, &call_id).await,
        "installation_disconnected"
    );
    assert!(setup.server.providers.tool_calls().is_empty(), "nothing sent");
}

#[tokio::test]
async fn refuses_an_approved_call_whose_tool_was_pinned_again() {
    let setup = setup().await;
    let key = api_key(&setup.server, &setup.org, "sandbox").await;
    let (run, call_id) = setup.waiting_on_send().await;
    // The server changes `send`, and an owner accepts the change: the pinned
    // definition the approval was asked about is no longer the one in force.
    setup.server.providers.serve_mcp(
        MCP,
        json!([
            { "name": "lookup", "description": "Look something up", "inputSchema": { "type": "object" }, "annotations": { "readOnlyHint": true } },
            { "name": "send", "description": "Send it to everyone", "inputSchema": { "type": "object" } },
        ]),
    );
    let (org, ws) = setup.scope();
    zupflow_server::discovery::enqueue(
        &setup.server.state,
        org,
        ws,
        InstallationId::parse(&setup.installation).unwrap(),
    )
    .await
    .unwrap();
    while zupflow_server::discovery::work_once(&setup.server.state).await {}
    let tools = format!("/v1/workspaces/{}/installations/{}", setup.sandbox, setup.installation);
    let (status, listed) = setup
        .server
        .send(call("alice", Method::GET, format!("{tools}/tools")).naming(&setup.org))
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let latest = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "send")
        .unwrap()["latest_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, body) = setup
        .server
        .send(
            call("alice", Method::POST, format!("{tools}/accept-tool-change"))
                .naming(&setup.org)
                .body(json!({ "name": "send", "latest_hash": latest })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(setup.approve_and_finish(&key, &run, &call_id).await, "tool_changed");
    assert!(setup.server.providers.tool_calls().is_empty(), "nothing sent");
}

#[tokio::test]
async fn refuses_a_call_whose_installation_cant_be_reached_as_set_up() {
    let setup = setup().await;
    // The app's Sandbox configuration no longer names a server.
    let (status, body) = setup
        .server
        .send(
            call(
                "alice",
                Method::PUT,
                format!(
                    "/v1/organizations/{}/apps/{}/environments/sandbox/configuration",
                    setup.org, setup.app
                ),
            )
            .body(json!({ "mcp_server_url": null })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let run = setup.run(&setup.calling("lookup", "{}")).await;
    let done = setup.get(&run).await;
    assert_eq!(done["status"], "completed", "the run goes on: {done}");
    let failed = setup
        .events(&run)
        .await
        .into_iter()
        .find(|event| event["type"] == "tool_call.failed")
        .unwrap();
    assert_eq!(failed["data"]["error"]["code"], "tool_refused");
    assert_eq!(
        failed["data"]["error"]["message"],
        "Main can't be used as it's set up: check its app's configuration, or reconnect it."
    );
    assert!(setup.server.providers.tool_calls().is_empty(), "nothing sent");
}

/// A run waiting for approval on `send` in another organization: Larkspur's
/// Sandbox workspace, with an installation of its own app and an agent.
struct Elsewhere {
    org: String,
    ws: String,
    run: String,
    call_id: String,
    key: String,
}

/// The short ID of `installation` in `ws`.
async fn short_id_of(server: &Server, who: &str, org: &str, ws: &str, installation: &str) -> String {
    let (status, read) = server
        .send(
            call(
                who,
                Method::GET,
                format!("/v1/workspaces/{ws}/installations/{installation}"),
            )
            .naming(org),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{read}");
    read["data"]["short_id"].as_str().unwrap().to_owned()
}

/// Starts a run of `agent` in `ws` with `key` calling `send` through
/// `installation`, works it until it parks: its ID and the call's.
async fn waiting_in(
    setup: &Setup,
    org: &str,
    ws: &str,
    agent: &str,
    installation: &str,
    key: &str,
) -> (String, String) {
    let short_id = short_id_of(&setup.server, key, org, ws, installation).await;
    let (status, run) = setup
        .server
        .send(
            call(key, Method::POST, format!("/v1/workspaces/{ws}/agents/{agent}/runs"))
                .body(json!({ "input": format!("call tools_{short_id}_send {{}}") })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{run}");
    let run = id(&run["data"]).to_owned();
    setup.work().await;
    let (status, events) = setup
        .server
        .send(call(key, Method::GET, format!("/v1/workspaces/{ws}/runs/{run}/events")))
        .await;
    assert_eq!(status, StatusCode::OK, "{events}");
    let requested = events["data"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(requested["type"], "approval.requested", "{events}");
    (run, requested["data"]["call_id"].as_str().unwrap().to_owned())
}

async fn elsewhere(setup: &Setup) -> Elsewhere {
    let server = &setup.server;
    let org = server.organization("alice", "larkspur").await;
    let ws = id(&server.workspace("alice", &org, "sandbox", "sandbox").await).to_owned();
    server.default_model("alice", &org, "sandbox").await;
    let (status, created) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/apps")).body(json!({
                "kind": "mcp", "name": "Tools", "key": "tools", "description": "", "auth_type": "token",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let app = id(&created["data"]).to_owned();
    let (status, body) = server
        .send(
            call("alice", Method::PATCH, format!("/v1/organizations/{org}/apps/{app}")).body(json!({ "auth": {
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
                "alice",
                Method::PUT,
                format!("/v1/organizations/{org}/apps/{app}/environments/sandbox/configuration"),
            )
            .body(json!({ "mcp_server_url": MCP })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let installation = connect(server, "alice", &org, &ws, &app).await;
    levels(server, &org, &app, "allow", "approve").await;
    let agent = agent(server, &org, &ws, &installation).await;
    let key = api_key(server, &org, "sandbox").await;
    let (run, call_id) = waiting_in(setup, &org, &ws, &agent, &installation, &key).await;
    Elsewhere {
        org,
        ws,
        run,
        call_id,
        key,
    }
}

/// How the Sandbox installation goes away.
#[derive(Clone, Copy)]
enum GoesAway {
    Deleted,
    Disconnected,
}

impl Setup {
    async fn take_away(&self, how: GoesAway) {
        let installation = format!("/v1/workspaces/{}/installations/{}", self.sandbox, self.installation);
        let (status, body) = match how {
            GoesAway::Deleted => {
                self.server
                    .send(call("alice", Method::DELETE, installation).naming(&self.org))
                    .await
            }
            GoesAway::Disconnected => {
                self.server
                    .send(call("alice", Method::POST, format!("{installation}/disconnect")).naming(&self.org))
                    .await
            }
        };
        assert!(status.is_success(), "{status}: {body}");
    }

    /// The Production workspace's installation.
    async fn production_installation(&self) -> String {
        let (status, listed) = self
            .server
            .send(
                call(
                    "alice",
                    Method::GET,
                    format!("/v1/workspaces/{}/installations", self.production),
                )
                .naming(&self.org),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{listed}");
        id(&listed["data"][0]).to_owned()
    }
}

/// Decision 0047, case 1: deleting or disconnecting an installation refuses
/// the call a run waits on for approval through it, with why, and the run
/// moves on without anyone deciding; an approval that comes later answers
/// `409 not_waiting`. Nothing reaches the app's server, and runs waiting in
/// the organization's other workspace and in another organization still
/// wait, and go on once approved.
async fn refuses_waiting_calls_once_the_installation_goes(how: GoesAway, code_expected: &str, verb: &str) {
    let setup = setup().await;
    let key = api_key(&setup.server, &setup.org, "sandbox").await;
    let production_key = api_key(&setup.server, &setup.org, "production").await;
    let (run, call_id) = setup.waiting_on_send().await;
    // A second run waiting on the same installation.
    let (other_run, other_call) = setup.waiting_on_send().await;
    let production_installation = setup.production_installation().await;
    let (production_run, production_call) = waiting_in(
        &setup,
        &setup.org,
        &setup.production,
        &setup.production_agent,
        &production_installation,
        &production_key,
    )
    .await;
    let theirs = elsewhere(&setup).await;

    setup.take_away(how).await;
    setup.work().await;

    for (run, call_id) in [(&run, &call_id), (&other_run, &other_call)] {
        let done = setup.get(run).await;
        assert_eq!(done["status"], "completed", "the run moved on: {done}");
        let message = format!("Main was {verb}, so this call wasn't made.");
        assert_eq!(
            done["output"],
            format!("{} failed: {message}", setup.tool("send")),
            "the model read why"
        );
        let events = setup.events(run).await;
        let types: Vec<&str> = events.iter().map(|event| event["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            [
                "run.queued",
                "run.started",
                "message.assistant",
                "approval.requested",
                "tool_call.failed",
                "message.assistant",
                "run.completed"
            ],
            "nobody decided it, and it was never sent"
        );
        let failed = &events[4];
        assert_eq!(failed["data"]["call_id"], call_id.as_str());
        assert_eq!(failed["data"]["error"]["code"], code_expected);
        assert_eq!(failed["data"]["error"]["message"], message.as_str());
        assert_eq!(failed["data"]["installation_label"], "Main");
        assert_eq!(done["usage"]["tool_calls"], 0);
        // Too late to decide.
        let (status, body) = setup.resolve(&key, run, call_id, "approved").await;
        assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "not_waiting"), "{body}");
        let call = setup.call_of(RunId::parse(run).unwrap()).await;
        assert_eq!((call.state, call.approval), (CallState::Refused, None));
    }
    assert!(setup.server.providers.tool_calls().is_empty(), "nothing was sent");

    // The organization's other workspace, and another organization: still
    // waiting, and approved, sent through their own installations.
    let (status, waiting) = setup
        .server
        .send(call(
            &production_key,
            Method::GET,
            format!("/v1/workspaces/{}/runs/{production_run}", setup.production),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(waiting["data"]["status"], "waiting_for_approval");
    let (status, waiting) = setup
        .server
        .send(call(
            &theirs.key,
            Method::GET,
            format!("/v1/workspaces/{}/runs/{}", theirs.ws, theirs.run),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(waiting["data"]["status"], "waiting_for_approval");
    for (key, ws, run, call_id) in [
        (&production_key, &setup.production, &production_run, &production_call),
        (&theirs.key, &theirs.ws, &theirs.run, &theirs.call_id),
    ] {
        let (status, body) = setup
            .server
            .send(
                call(key, Method::POST, format!("/v1/workspaces/{ws}/runs/{run}/approvals"))
                    .body(json!({ "call_id": call_id, "decision": "approved" })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    setup.work().await;
    for (key, ws, run) in [
        (&production_key, &setup.production, &production_run),
        (&theirs.key, &theirs.ws, &theirs.run),
    ] {
        let (_, done) = setup
            .server
            .send(call(key, Method::GET, format!("/v1/workspaces/{ws}/runs/{run}")))
            .await;
        assert_eq!(done["data"]["status"], "completed", "{done}");
        assert_eq!(done["data"]["usage"]["tool_calls"], 1);
    }
    assert_eq!(setup.server.providers.tool_calls().len(), 2, "only the approved calls");
    assert!(!theirs.org.is_empty());
}

#[tokio::test]
async fn refuses_the_calls_waiting_on_a_deleted_installation() {
    refuses_waiting_calls_once_the_installation_goes(GoesAway::Deleted, "installation_removed", "removed").await;
}

#[tokio::test]
async fn refuses_the_calls_waiting_on_a_disconnected_installation() {
    refuses_waiting_calls_once_the_installation_goes(
        GoesAway::Disconnected,
        "installation_disconnected",
        "disconnected",
    )
    .await;
}

/// A call further on in the turn, waiting for approval on an installation
/// that went while the run waited on another: refused when the run gets to
/// it, never asked about.
#[tokio::test]
async fn never_asks_about_a_call_whose_installation_went_before_the_run_got_to_it() {
    let setup = setup().await;
    let key = api_key(&setup.server, &setup.org, "sandbox").await;
    let run = setup
        .run(&format!(
            "{}\n{}",
            setup.calling("send", "{\"n\":1}"),
            setup.calling("send", "{\"n\":2}")
        ))
        .await;
    let first = setup.events(&run).await.last().unwrap()["data"]["call_id"]
        .as_str()
        .unwrap()
        .to_owned();
    // The first is approved before the installation goes; the second hasn't
    // been asked about yet.
    let (status, _) = setup.resolve(&key, &run, &first, "approved").await;
    assert_eq!(status, StatusCode::OK);
    setup.take_away(GoesAway::Deleted).await;
    setup.work().await;
    let done = setup.get(&run).await;
    assert_eq!(done["status"], "completed", "{done}");
    let events = setup.events(&run).await;
    let asked = events
        .iter()
        .filter(|event| event["type"] == "approval.requested")
        .count();
    assert_eq!(asked, 1, "the second call was never asked about");
    let codes: Vec<&str> = events
        .iter()
        .filter(|event| event["type"] == "tool_call.failed")
        .map(|event| event["data"]["error"]["code"].as_str().unwrap())
        .collect();
    assert_eq!(codes, ["installation_removed", "installation_removed"]);
    assert!(setup.server.providers.tool_calls().is_empty(), "nothing was sent");
}

/// Disconnected, then reconnected before the job ran: the installation is
/// the same row with the same short ID, so the call still waits, and its
/// approval still authorizes it.
#[tokio::test]
async fn keeps_a_call_waiting_on_an_installation_reconnected_since() {
    let setup = setup().await;
    let key = api_key(&setup.server, &setup.org, "sandbox").await;
    let (run, call_id) = setup.waiting_on_send().await;
    setup.take_away(GoesAway::Disconnected).await;
    let (status, session) = setup
        .server
        .send(
            call("alice", Method::POST, format!("/v1/workspaces/{}/connect-sessions", setup.sandbox))
                .naming(&setup.org)
                .body(json!({ "app_id": setup.app, "intent": "reconnect", "label": "Main", "installation_id": setup.installation, "return_url": null })),
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
    let (status, done) = setup
        .server
        .send(
            call(
                "nobody",
                Method::POST,
                format!("/v1/connect/sessions/{reference}/credentials"),
            )
            .body(json!({ "values": { "api_key": "tk_1234" } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(
        short_id_of(&setup.server, "alice", &setup.org, &setup.sandbox, &setup.installation).await,
        setup.short_id,
        "the same short ID"
    );
    setup.work().await;
    assert_eq!(setup.get(&run).await["status"], "waiting_for_approval", "still waiting");
    let (status, body) = setup.resolve(&key, &run, &call_id, "approved").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    setup.work().await;
    let done = setup.get(&run).await;
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(done["usage"]["tool_calls"], 1);
    assert_eq!(setup.server.providers.tool_calls().len(), 1);
}

/// While a call waited for approval, its grant was removed, or the
/// organization denied its tool: the approval that comes then doesn't send
/// it; it's refused at send, and the run goes on.
#[tokio::test]
async fn refuses_an_approved_call_whose_grant_or_level_went_while_it_waited() {
    for what in ["grant", "level"] {
        let setup = setup().await;
        let key = api_key(&setup.server, &setup.org, "sandbox").await;
        let (run, call_id) = setup.waiting_on_send().await;
        match what {
            "grant" => {
                let grants = format!("/v1/workspaces/{}/agents/{}/grants", setup.sandbox, setup.agent);
                let (_, listed) = setup
                    .server
                    .send(call("alice", Method::GET, grants.clone()).naming(&setup.org))
                    .await;
                let grant = listed["data"][0]["id"].as_str().unwrap().to_owned();
                let (status, _) = setup
                    .server
                    .send(call("alice", Method::DELETE, format!("{grants}/{grant}")).naming(&setup.org))
                    .await;
                assert_eq!(status, StatusCode::NO_CONTENT);
            }
            _ => levels(&setup.server, &setup.org, &setup.app, "allow", "deny").await,
        }
        assert_eq!(
            setup.approve_and_finish(&key, &run, &call_id).await,
            "tool_refused",
            "{what}"
        );
        assert!(setup.server.providers.tool_calls().is_empty(), "{what}: nothing sent");
    }
}

/// Deleted and installed again: the new installation has a new short ID, a
/// call by the old name reaches nothing, and the approval that came for the
/// old installation's call answers `409 not_waiting`.
#[tokio::test]
async fn never_reaches_a_reinstalled_installation_through_the_old_one() {
    let setup = setup().await;
    let key = api_key(&setup.server, &setup.org, "sandbox").await;
    let (run, call_id) = setup.waiting_on_send().await;
    setup.take_away(GoesAway::Deleted).await;
    let again = connect(&setup.server, "alice", &setup.org, &setup.sandbox, &setup.app).await;
    let short_id = short_id_of(&setup.server, "alice", &setup.org, &setup.sandbox, &again).await;
    assert_ne!(short_id, setup.short_id, "a new short ID");
    let grants = format!("/v1/workspaces/{}/agents/{}/grants", setup.sandbox, setup.agent);
    let (status, body) = setup
        .server
        .send(
            call("alice", Method::POST, grants)
                .naming(&setup.org)
                .body(json!({ "installation_id": again, "tools": ["lookup", "send"], "approval_required": ["send"] })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    setup.work().await;
    let (status, body) = setup.resolve(&key, &run, &call_id, "approved").await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "not_waiting"), "{body}");
    // The old name isn't offered any more.
    let old = setup.run(&setup.calling("lookup", "{}")).await;
    let done = setup.get(&old).await;
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(
        done["output"],
        format!("I can't use {}: it isn't one of my tools.", setup.tool("lookup")),
        "the model wasn't offered it"
    );
    assert!(
        setup.server.providers.tool_calls().is_empty(),
        "nothing reached the new installation"
    );
}
