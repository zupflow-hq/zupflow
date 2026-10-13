//! Model providers through the API (decisions 0049, 0050): a connection's key
//! is write-only everywhere it could surface, each organization and each
//! environment reaches only its own connections, models and default, and a
//! run calls its model only while its model, connection and key still hold.

#![allow(clippy::unwrap_used)]

mod common;

use axum::http::{Method, StatusCode};
use common::{DEFAULT_MODEL, PROVIDER_KEY, Server, call, code};
use serde_json::{Value, json};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{ModelConnectionId, OrganizationId};
use zupflow_server::run_engine;

fn id(value: &Value) -> &str {
    value["id"].as_str().unwrap()
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

async fn connect(server: &Server, who: &str, org: &str, environment: &str, key: &str) -> (StatusCode, Value) {
    server
        .send(
            call(who, Method::POST, format!("/v1/organizations/{org}/model-connections")).body(json!({
                "environment": environment, "kind": "anthropic", "label": "Anthropic", "api_key": key,
            })),
        )
        .await
}

/// Every byte the test server's database holds, its write-ahead log included.
fn stored_bytes(server: &Server) -> Vec<u8> {
    let mut bytes = Vec::new();
    for entry in std::fs::read_dir(&server.directory).unwrap() {
        bytes.extend(std::fs::read(entry.unwrap().path()).unwrap());
    }
    bytes
}

fn holds(haystack: &[u8], needle: &str) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle.as_bytes())
}

#[tokio::test]
async fn never_shows_or_stores_a_key_as_given() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let first = "sk-ant-first-0123456789abcdQRST";
    let second = "sk-ant-second-0123456789abcdUVWX";
    let refused = "sk-ant-invalid-0123456789abcdYZAB";

    let created = |status: StatusCode, body: &Value| {
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert!(!body.to_string().contains(first), "never answered");
        assert_eq!(
            body["data"]["api_key"],
            json!({ "set": true, "last4": "QRST", "updated_at": body["data"]["api_key"]["updated_at"] })
        );
        assert_eq!(body["data"]["key_version"], 1);
    };
    let request = || {
        call(
            "alice",
            Method::POST,
            format!("/v1/organizations/{org}/model-connections"),
        )
        .idempotent("connect-once")
        .body(json!({ "environment": "sandbox", "kind": "anthropic", "label": "Anthropic", "api_key": first }))
    };
    let (status, body) = server.send(request()).await;
    created(status, &body);
    // A retry answers what the first request did, without the key.
    let (status, replay) = server.send(request()).await;
    created(status, &replay);
    assert_eq!(id(&replay["data"]), id(&body["data"]));
    let connection = id(&body["data"]).to_owned();

    // A refused key is named by what's wrong, never repeated.
    let (status, answer) = connect(&server, "alice", &org, "sandbox", refused).await;
    assert_eq!(
        (status, code(&answer)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_api_key")
    );
    assert!(answer["error"]["fields"]["api_key"].is_string());
    assert!(!answer.to_string().contains(refused));
    for (key, field) in [
        ("short", "Paste the whole key."),
        ("sk-ant with spaces 0123456789", "A key has no spaces."),
    ] {
        let (status, answer) = connect(&server, "alice", &org, "sandbox", key).await;
        assert_eq!(
            (status, &answer["error"]["fields"]["api_key"]),
            (StatusCode::BAD_REQUEST, &json!(field))
        );
        assert!(!answer.to_string().contains(key));
    }

    // Replacing it stores the next version and keeps the old one nowhere.
    let key_path = format!("/v1/organizations/{org}/model-connections/{connection}/api-key");
    let (status, replaced) = server
        .send(call("alice", Method::PUT, &key_path).body(json!({ "api_key": second })))
        .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    assert_eq!(
        (&replaced["data"]["key_version"], &replaced["data"]["api_key"]["last4"]),
        (&json!(2), &json!("UVWX"))
    );
    let (status, answer) = server
        .send(call("alice", Method::PUT, &key_path).body(json!({ "api_key": refused })))
        .await;
    assert_eq!(
        (status, code(&answer)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_api_key")
    );
    let (_, read) = server
        .send(call(
            "alice",
            Method::GET,
            format!("/v1/organizations/{org}/model-connections/{connection}"),
        ))
        .await;
    assert_eq!(
        (&read["data"]["key_version"], &read["data"]["status"]),
        (&json!(2), &json!("active")),
        "a refused replacement changes nothing"
    );

    // The key opens to ask the provider, and nothing it lists repeats it.
    let (status, listed) = server
        .send(call(
            "alice",
            Method::GET,
            format!("/v1/organizations/{org}/model-connections/{connection}/provider-models"),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(!listed["data"].as_array().unwrap().is_empty());

    let (status, audit) = server
        .send(call(
            "alice",
            Method::GET,
            format!("/v1/organizations/{org}/audit-events"),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{audit}");
    let actions: Vec<&str> = audit["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["action"].as_str().unwrap())
        .collect();
    assert!(actions.contains(&"model_connection.created") && actions.contains(&"model_connection.key_replaced"));
    let everything = [body, replay, replaced, read, listed, audit]
        .iter()
        .map(Value::to_string)
        .collect::<String>();
    for key in [first, second, refused] {
        assert!(!everything.contains(key), "no answer holds a key");
        assert!(
            !holds(&stored_bytes(&server), key),
            "the database holds keys only sealed"
        );
    }
}

#[tokio::test]
async fn keeps_organizations_and_environments_apart() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let theirs = server.organization("mallory", "larkspur").await;
    let sandbox_model = server.default_model("alice", &org, "sandbox").await;
    let production_model = server.default_model("alice", &org, "production").await;
    let (_, connections) = server
        .send(call(
            "alice",
            Method::GET,
            format!("/v1/organizations/{org}/model-connections"),
        ))
        .await;
    let connection_in = |environment: &str| {
        connections["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|connection| connection["environment"] == environment)
            .map(|connection| id(connection).to_owned())
            .unwrap()
    };
    let (sandbox_connection, production_connection) = (connection_in("sandbox"), connection_in("production"));

    // Another organization's member finds nothing of it, by any ID.
    for path in [
        format!("/v1/organizations/{org}/model-connections"),
        format!("/v1/organizations/{org}/model-connections/{sandbox_connection}"),
        format!("/v1/organizations/{org}/models/{sandbox_model}"),
        format!("/v1/organizations/{org}/default-models/sandbox"),
        format!("/v1/organizations/{theirs}/model-connections/{sandbox_connection}"),
        format!("/v1/organizations/{theirs}/models/{sandbox_model}"),
    ] {
        let (status, body) = server.send(call("mallory", Method::GET, &path)).await;
        assert_eq!((status, code(&body)), (StatusCode::NOT_FOUND, "not_found"), "{path}");
    }
    let (status, theirs_listed) = server
        .send(call(
            "mallory",
            Method::GET,
            format!("/v1/organizations/{theirs}/models"),
        ))
        .await;
    assert_eq!((status, &theirs_listed["data"]), (StatusCode::OK, &json!([])));
    let (status, body) = server
        .send(
            call(
                "mallory",
                Method::PUT,
                format!("/v1/organizations/{theirs}/default-models/sandbox"),
            )
            .body(json!({ "model_id": sandbox_model })),
        )
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "unknown_model")
    );

    // A Sandbox key reaches Sandbox's alone.
    let key = api_key(&server, &org, "sandbox").await;
    let (status, listed) = server
        .send(call(
            &key,
            Method::GET,
            format!("/v1/organizations/{org}/model-connections"),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = listed["data"].as_array().unwrap().iter().map(id).collect();
    assert_eq!(ids, [sandbox_connection.as_str()]);
    let (_, models) = server
        .send(call(&key, Method::GET, format!("/v1/organizations/{org}/models")))
        .await;
    let ids: Vec<&str> = models["data"].as_array().unwrap().iter().map(id).collect();
    assert_eq!(ids, [sandbox_model.as_str()]);
    for path in [
        format!("/v1/organizations/{org}/model-connections/{production_connection}"),
        format!("/v1/organizations/{org}/model-connections/{production_connection}/provider-models"),
        format!("/v1/organizations/{org}/models/{production_model}"),
    ] {
        let (status, body) = server.send(call(&key, Method::GET, &path)).await;
        assert_eq!((status, code(&body)), (StatusCode::NOT_FOUND, "not_found"), "{path}");
    }
    let (status, body) = server
        .send(call(
            &key,
            Method::DELETE,
            format!("/v1/organizations/{org}/models/{production_model}"),
        ))
        .await;
    assert_eq!((status, code(&body)), (StatusCode::NOT_FOUND, "not_found"));
    for (method, path, body) in [
        (
            Method::GET,
            format!("/v1/organizations/{org}/default-models/production"),
            None,
        ),
        (
            Method::PUT,
            format!("/v1/organizations/{org}/default-models/production"),
            Some(json!({ "model_id": null })),
        ),
        (
            Method::POST,
            format!("/v1/organizations/{org}/model-connections"),
            Some(json!({ "environment": "production", "kind": "openai", "label": "OpenAI", "api_key": PROVIDER_KEY })),
        ),
    ] {
        let mut request = call(&key, method, &path);
        if let Some(body) = body {
            request = request.body(body);
        }
        let (status, answer) = server.send(request).await;
        assert_eq!(
            (status, code(&answer)),
            (StatusCode::UNPROCESSABLE_ENTITY, "environment_mismatch"),
            "{path}"
        );
    }
    // Nothing falls back across environments: Sandbox's default can't be
    // one of Production's models.
    let (status, body) = server
        .send(
            call(
                &key,
                Method::PUT,
                format!("/v1/organizations/{org}/default-models/sandbox"),
            )
            .body(json!({ "model_id": production_model })),
        )
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "unknown_model")
    );
    let (_, production) = server
        .send(call(
            "alice",
            Method::GET,
            format!("/v1/organizations/{org}/default-models/production"),
        ))
        .await;
    assert_eq!(production["data"]["model"]["id"], json!(production_model), "untouched");
}

/// A Sandbox workspace with an agent and a default model: the workspace, the
/// agent and the default's ID.
async fn sandbox_agent(server: &Server, org: &str) -> (String, String, String) {
    let ws = id(&server.workspace("alice", org, "sandbox", "sandbox").await).to_owned();
    let model = server.default_model("alice", org, "sandbox").await;
    let (status, agent) = server
        .send(
            call("alice", Method::POST, format!("/v1/workspaces/{ws}/agents"))
                .naming(org)
                .body(json!({ "name": "Helper", "description": "", "instructions": "Be brief." })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{agent}");
    (ws, id(&agent["data"]).to_owned(), model)
}

async fn start(server: &Server, org: &str, ws: &str, agent: &str) -> (StatusCode, Value) {
    server
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/workspaces/{ws}/agents/{agent}/runs"),
            )
            .naming(org)
            .body(json!({ "input": "hello" })),
        )
        .await
}

async fn finished(server: &Server, org: &str, ws: &str, run: &str) -> Value {
    while run_engine::work_once(&server.state).await {}
    let (status, body) = server
        .send(call("alice", Method::GET, format!("/v1/workspaces/{ws}/runs/{run}")).naming(org))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"].clone()
}

#[tokio::test]
async fn runs_on_the_agents_model_or_else_the_default() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let ws = id(&server.workspace("alice", &org, "sandbox", "sandbox").await).to_owned();
    let (_, agent) = server
        .send(
            call("alice", Method::POST, format!("/v1/workspaces/{ws}/agents"))
                .naming(&org)
                .body(json!({ "name": "Helper", "description": "", "instructions": "" })),
        )
        .await;
    let agent = id(&agent["data"]).to_owned();
    // Neither an override nor a default: refused, and nothing falls back to
    // Production's.
    server.default_model("alice", &org, "production").await;
    let (status, body) = start(&server, &org, &ws, &agent).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "model_not_configured")
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Sandbox has no default model")
    );

    let default = server.default_model("alice", &org, "sandbox").await;
    let (status, run) = start(&server, &org, &ws, &agent).await;
    assert_eq!(status, StatusCode::CREATED, "{run}");
    let done = finished(&server, &org, &ws, id(&run["data"])).await;
    assert_eq!(done["status"], "completed", "{done}");
    let (_, events) = server
        .send(
            call(
                "alice",
                Method::GET,
                format!("/v1/workspaces/{ws}/runs/{}/events", id(&run["data"])),
            )
            .naming(&org),
        )
        .await;
    let started = events["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"] == "run.started")
        .unwrap();
    assert_eq!(
        (&started["data"]["model"], &started["data"]["model_id"]),
        (&json!(DEFAULT_MODEL), &json!(default))
    );
}

#[tokio::test]
async fn fails_a_run_whose_model_went_before_it_was_called() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let (ws, agent, default) = sandbox_agent(&server, &org).await;
    // The agent's own model, from a connection of its own.
    let (status, connection) = connect(&server, "alice", &org, "sandbox", PROVIDER_KEY).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, own) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/models")).body(json!({
                "connection_id": id(&connection["data"]), "provider_model_id": "claude-haiku-4-5",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{own}");
    let agent_path = format!("/v1/workspaces/{ws}/agents/{agent}");
    let (status, _) = server
        .send(
            call("alice", Method::PATCH, &agent_path)
                .naming(&org)
                .body(json!({ "model_id": id(&own["data"]) })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, run) = start(&server, &org, &ws, &agent).await;
    assert_eq!(status, StatusCode::CREATED);
    // Before any worker takes it, the agent lets the model go and it's removed.
    server
        .send(
            call("alice", Method::PATCH, &agent_path)
                .naming(&org)
                .body(json!({ "model_id": null })),
        )
        .await;
    let (status, _) = server
        .send(call(
            "alice",
            Method::DELETE,
            format!("/v1/organizations/{org}/models/{}", id(&own["data"])),
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let failed = finished(&server, &org, &ws, id(&run["data"])).await;
    assert_eq!(
        (&failed["status"], &failed["error"]["code"]),
        (&json!("failed"), &json!("model_unavailable")),
        "{failed}"
    );
    // The next run takes the default.
    let (_, next) = start(&server, &org, &ws, &agent).await;
    let done = finished(&server, &org, &ws, id(&next["data"])).await;
    assert_eq!(done["status"], "completed");
    assert_ne!(default, id(&own["data"]));
}

#[tokio::test]
async fn fails_a_run_whose_key_the_provider_refused() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let (ws, agent, _) = sandbox_agent(&server, &org).await;
    let (status, run) = start(&server, &org, &ws, &agent).await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, connections) = server
        .send(call(
            "alice",
            Method::GET,
            format!("/v1/organizations/{org}/model-connections"),
        ))
        .await;
    let connection = id(&connections["data"][0]).to_owned();
    server
        .state
        .model_store
        .mark_key_refused(
            OrganizationId::parse(&org).unwrap(),
            Environment::Sandbox,
            ModelConnectionId::parse(&connection).unwrap(),
            1,
        )
        .await
        .unwrap();
    let failed = finished(&server, &org, &ws, id(&run["data"])).await;
    assert_eq!(
        (&failed["status"], &failed["error"]["code"]),
        (&json!("failed"), &json!("model_key_invalid")),
        "{failed}"
    );
    assert!(failed["error"]["message"].as_str().unwrap().contains("OpenAI"));

    // A new key makes the connection usable again.
    let (status, _) = server
        .send(
            call(
                "alice",
                Method::PUT,
                format!("/v1/organizations/{org}/model-connections/{connection}/api-key"),
            )
            .body(json!({ "api_key": "sk-test-replacement-0123456789" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, next) = start(&server, &org, &ws, &agent).await;
    let done = finished(&server, &org, &ws, id(&next["data"])).await;
    assert_eq!(done["status"], "completed", "{done}");
}

#[tokio::test]
async fn fails_a_run_waiting_on_a_model_whose_route_went() {
    use zupflow_core::ids::{RunId, WorkspaceId};
    use zupflow_core::model_store::RouteState;
    use zupflow_core::runs::names::build_offer;
    use zupflow_core::runs::reducer::{Facts, Io, Limits, Plan, advance};
    use zupflow_core::runs::state::ClaimantId;
    use zupflow_core::runs::store::Applied;
    use zupflow_core::time::Timestamp;

    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let (ws, agent, _) = sandbox_agent(&server, &org).await;
    let (status, connection) = connect(&server, "alice", &org, "sandbox", PROVIDER_KEY).await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, own) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/models")).body(json!({
                "connection_id": id(&connection["data"]), "provider_model_id": "claude-haiku-4-5",
            })),
        )
        .await;
    let agent_path = format!("/v1/workspaces/{ws}/agents/{agent}");
    server
        .send(
            call("alice", Method::PATCH, &agent_path)
                .naming(&org)
                .body(json!({ "model_id": id(&own["data"]) })),
        )
        .await;
    let (status, run) = start(&server, &org, &ws, &agent).await;
    assert_eq!(status, StatusCode::CREATED);

    // A worker claims the run and asks the model, then stops: the run waits
    // in `AwaitModel` with its claim lapsed.
    let state = &server.state;
    let (org_id, ws_id) = (OrganizationId::parse(&org).unwrap(), WorkspaceId::parse(&ws).unwrap());
    let run_id = RunId::parse(id(&run["data"])).unwrap();
    let claimant = ClaimantId::from_uuid(uuid::Uuid::now_v7());
    let lapsing = Limits {
        lease_ms: 1,
        send_lease_ms: 1,
        ..Limits::default()
    };
    let step = |claim: Option<u64>, offer, limits| async move {
        let snapshot = state.runs.find_snapshot(org_id, ws_id, run_id).await.unwrap().unwrap();
        let facts = Facts {
            claimant: Some(claimant),
            claim,
            offer,
            model_route: Some(RouteState::Usable { key_version: 1 }),
            limits,
            ..Facts::at(Timestamp::now())
        };
        let Plan::Apply { request, then } = advance(&snapshot, &facts).unwrap() else {
            panic!("a step to take");
        };
        assert!(matches!(
            state.runs.apply(org_id, ws_id, &request).await.unwrap(),
            Applied::Committed(_)
        ));
        then
    };
    step(None, None, Limits::default()).await;
    let asked = step(Some(1), Some(build_offer([]).offer), lapsing).await;
    assert!(matches!(asked, Io::CallModel(_)), "the model is asked");
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    // Meanwhile the agent lets its model go, and the model is removed.
    server
        .send(
            call("alice", Method::PATCH, &agent_path)
                .naming(&org)
                .body(json!({ "model_id": null })),
        )
        .await;
    let (status, _) = server
        .send(call(
            "alice",
            Method::DELETE,
            format!("/v1/organizations/{org}/models/{}", id(&own["data"])),
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let failed = finished(&server, &org, &ws, id(&run["data"])).await;
    assert_eq!(
        (&failed["status"], &failed["error"]["code"], &failed["error"]["message"]),
        (
            &json!("failed"),
            &json!("model_unavailable"),
            &json!("The run's model was removed while the run was under way.")
        ),
        "{failed}"
    );
}

#[tokio::test]
async fn says_so_for_good_when_the_deployment_reaches_no_providers() {
    let mut server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let (status, connection) = connect(&server, "alice", &org, "sandbox", PROVIDER_KEY).await;
    assert_eq!(status, StatusCode::CREATED);
    let connection = id(&connection["data"]).to_owned();
    let mut state = server.state.clone();
    state.model_catalog = std::sync::Arc::new(zupflow_basic::no_providers::NoProviders);
    server.app = zupflow_server::app(state, zupflow_server::RequestGuard::default());

    let base = format!("/v1/organizations/{org}/model-connections");
    for request in [
        call("alice", Method::POST, &base).body(json!({
            "environment": "sandbox", "kind": "openai", "label": "OpenAI", "api_key": PROVIDER_KEY,
        })),
        call("alice", Method::PUT, format!("{base}/{connection}/api-key"))
            .body(json!({ "api_key": "sk-ant-another-0123456789abcd" })),
        call("alice", Method::GET, format!("{base}/{connection}/provider-models")),
    ] {
        let uri = request.uri.clone();
        let (status, body) = server.send(request).await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::NOT_IMPLEMENTED, "model_providers_not_configured"),
            "{uri}: {body}"
        );
        assert!(!body.to_string().contains(PROVIDER_KEY));
    }
    let (_, listed) = server.send(call("alice", Method::GET, &base)).await;
    assert_eq!(listed["data"].as_array().unwrap().len(), 1, "nothing saved");
    assert_eq!(listed["data"][0]["key_version"], 1, "the key kept");
}
