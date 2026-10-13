//! Runs answered through the real provider adapters (decision 0049), over
//! the providers' APIs faked in process (`zupflow_providers::fake`): no real
//! provider is reached. Each kind carries a tool-calling conversation to
//! its end; a connection's key is opened only for the call and never
//! surfaces, even from a provider that repeats it back; and a run meets
//! the provider's faults as the plan says: a key refused (`401`) mid-run
//! fails it `model_key_invalid` and marks the connection, unless the key was
//! replaced meanwhile; a model gone upstream fails it `model_unavailable`
//! and marks the model; `402` and `403` fail it `model_failed` and mark
//! nothing; `429` and `5xx` are tried again through the job's attempts,
//! until they run out, and a timeout once.

#![allow(clippy::unwrap_used)]

mod common;
#[path = "common/faults.rs"]
mod faults;
#[path = "common/run_setup.rs"]
mod run_setup;

use std::sync::{Arc, Mutex, OnceLock};

use axum::http::{Method, StatusCode};
use common::{DEFAULT_MODEL, PROVIDER_KEY, Server, call};
use faults::Worker;
use run_setup::{Setup, id, setup_on};
use serde_json::{Value, json};
use zupflow_core::model_providers::ProviderKind;
use zupflow_core::runs::store::RUN_JOB_ATTEMPTS;
use zupflow_providers::MODEL_CALL_LIMITS;
use zupflow_providers::fake::Fault;

async fn setup() -> Setup {
    setup_on(Server::with_model_providers().await, &[("Main", "tk_main")]).await
}

/// A model each kind serves that calls tools.
fn served(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenAi => DEFAULT_MODEL,
        ProviderKind::Anthropic => "claude-sonnet-4-5",
        ProviderKind::OpenRouter => "openai/gpt-5-mini",
    }
}

impl Setup {
    /// Connects `kind` in Sandbox with `key`, adds `model` from it and
    /// makes it the default. The connection's and the model's IDs.
    async fn route(&self, kind: ProviderKind, key: &str, model: &str) -> (String, String) {
        let (status, connection) = self
            .kit
            .send(
                call(
                    "alice",
                    Method::POST,
                    format!("/v1/organizations/{}/model-connections", self.org),
                )
                .body(json!({
                    "environment": "sandbox", "kind": kind.as_str(), "label": kind.name(), "api_key": key,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{connection}");
        let connection = id(&connection["data"]).to_owned();
        let (status, added) = self
            .kit
            .send(
                call("alice", Method::POST, format!("/v1/organizations/{}/models", self.org))
                    .body(json!({ "connection_id": connection, "provider_model_id": model })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{added}");
        let model = id(&added["data"]).to_owned();
        let (status, body) = self
            .kit
            .send(
                call(
                    "alice",
                    Method::PUT,
                    format!("/v1/organizations/{}/default-models/sandbox", self.org),
                )
                .body(json!({ "model_id": model })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        (connection, model)
    }

    /// The Sandbox default's model and connection, as read.
    async fn default_route(&self) -> (String, String) {
        let (status, body) = self
            .kit
            .send(call(
                "alice",
                Method::GET,
                format!("/v1/organizations/{}/default-models/sandbox", self.org),
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let model = &body["data"]["model"];
        (
            id(model).to_owned(),
            model["route"]["connection_id"].as_str().unwrap().to_owned(),
        )
    }

    async fn read(&self, path: String) -> Value {
        let (status, body) = self.kit.send(call("alice", Method::GET, path)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["data"].clone()
    }

    async fn connection(&self, connection: &str) -> Value {
        self.read(format!("/v1/organizations/{}/model-connections/{connection}", self.org))
            .await
    }

    async fn model(&self, model: &str) -> Value {
        self.read(format!("/v1/organizations/{}/models/{model}", self.org))
            .await
    }

    /// Works every run job, retries included, until none is due.
    async fn work(&self) {
        self.kit.drain(Worker::A).await;
    }

    /// The key version each of the run's model attempts was prepared with,
    /// and its state, in order.
    async fn attempts(&self, run: zupflow_core::ids::RunId) -> Vec<(Option<i64>, String)> {
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite:{}",
            self.server.directory.join("zupflow.sqlite").display()
        ))
        .await
        .unwrap();
        let attempts =
            sqlx::query_as("SELECT key_version, state FROM run_model_attempts WHERE run_id = ? ORDER BY turn, attempt")
                .bind(run.uuid())
                .fetch_all(&pool)
                .await
                .unwrap();
        pool.close().await;
        attempts
    }
}

fn error_code(run: &Value) -> &str {
    run["error"]["code"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn carries_a_tool_calling_conversation_through_each_provider() {
    for kind in ProviderKind::ALL {
        let setup = setup().await;
        let key = format!("sk-{}-0123456789abcdefWXYZ", kind.as_str());
        if kind != ProviderKind::OpenAi {
            setup.route(kind, &key, served(kind)).await;
        }
        let key = if kind == ProviderKind::OpenAi {
            PROVIDER_KEY.to_owned()
        } else {
            key
        };
        let before = setup.server.models.turns().len();
        let run = setup.start(&setup.calling("lookup", "{\"q\": \"status\"}").await).await;
        setup.work().await;
        let finished = setup.get(run).await;
        assert_eq!(finished["status"], "completed", "{kind:?}: {finished}");
        assert!(
            finished["output"].as_str().unwrap().contains("lookup"),
            "{kind:?}: the model read the call's result: {finished}"
        );
        assert_eq!(finished["usage"]["tool_calls"], 1);
        let turns: Vec<_> = setup.server.models.turns().split_off(before);
        assert_eq!(turns.len(), 2, "{kind:?}: a call, then the answer");
        let target = zupflow_providers::endpoint(kind);
        let expected_credential = match kind {
            ProviderKind::Anthropic => ("x-api-key".to_owned(), key.clone()),
            _ => ("Authorization".to_owned(), format!("Bearer {key}")),
        };
        for turn in &turns {
            assert_eq!(turn.url, target.turn_url);
            assert_eq!(turn.limits, MODEL_CALL_LIMITS, "model calls get their own limits");
            assert_eq!(turn.credential.as_ref(), Some(&expected_credential));
            assert!(!turn.key_elsewhere, "the key is the credential and nothing else");
            assert_eq!(turn.body["model"], served(kind), "the route's own model, never another");
        }
        // Tokens are the provider's own count, as its answers said.
        let input = finished["usage"]["input_tokens"].as_u64().unwrap();
        let output = finished["usage"]["output_tokens"].as_u64().unwrap();
        assert!(input > 0 && output > 0, "{finished}");
        // The second turn carried the call and its result back.
        let second = turns[1].body.to_string();
        assert!(
            second.contains("tools_") && second.contains("lookup done"),
            "{kind:?}: {second}"
        );
        let attempts = setup.attempts(run).await;
        assert_eq!(
            attempts,
            [(Some(1), "answered".to_owned()), (Some(1), "answered".to_owned())],
            "each attempt records the key version it was sent with"
        );
    }
}

/// Collects every log line the test binary writes.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    /// The process's one subscriber, set on first use. A scoped one isn't
    /// enough: callsite interest is cached process-wide, so another test
    /// running at the same time with none can switch its events off.
    fn global() -> &'static Logs {
        static LOGS: OnceLock<Logs> = OnceLock::new();
        LOGS.get_or_init(|| {
            let logs = Logs::default();
            let writer = logs.clone();
            let subscriber = tracing_subscriber::fmt()
                .json()
                .with_max_level(tracing::Level::TRACE)
                .with_writer(move || writer.clone())
                .finish();
            tracing::subscriber::set_global_default(subscriber).expect("no other subscriber in this binary");
            logs
        })
    }
}

#[tokio::test]
async fn never_lets_a_key_out_even_when_the_provider_repeats_it() {
    // Every test's logs land here, which only widens what's checked.
    let logs = Logs::global();

    let setup = setup().await;
    setup.server.models.echo_key();
    let mut keys = vec![PROVIDER_KEY.to_owned()];
    let mut runs = Vec::new();
    for kind in ProviderKind::ALL {
        if kind != ProviderKind::OpenAi {
            let key = format!("sk-{}-echoed-0123456789abcd", kind.as_str());
            setup.route(kind, &key, served(kind)).await;
            keys.push(key);
        }
        let run = setup.start(&setup.calling("lookup", "{}").await).await;
        setup.work().await;
        let finished = setup.get(run).await;
        assert_eq!(finished["status"], "completed", "{kind:?}: {finished}");
        assert!(
            finished["output"].as_str().unwrap().contains("[redacted]"),
            "the provider did repeat the key: {finished}"
        );
        runs.push(run);
    }
    // And a key refused, whose error repeats it too.
    let second = "sk-openai-second-0123456789abcd";
    let (_, model) = setup.route(ProviderKind::OpenAi, second, "gpt-5").await;
    keys.push(second.to_owned());
    setup.server.models.revoke(second);
    let refused = setup.start("hello").await;
    setup.work().await;
    assert_eq!(error_code(&setup.get(refused).await), "model_key_invalid");
    runs.push(refused);

    let mut seen = Vec::new();
    for run in &runs {
        seen.push(setup.get(*run).await.to_string());
        seen.push(Value::Array(setup.events(*run).await).to_string());
    }
    for path in [
        format!("/v1/organizations/{}/model-connections", setup.org),
        format!("/v1/organizations/{}/models", setup.org),
        format!("/v1/organizations/{}/models/{model}", setup.org),
        format!("/v1/organizations/{}/audit-events", setup.org),
        format!("/v1/workspaces/{}/runs", setup.ws),
        format!("/v1/organizations/{}/usage?environment=sandbox", setup.org),
    ] {
        let (status, body) = setup
            .kit
            .send(call("alice", Method::GET, path).naming(&setup.org))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        seen.push(body.to_string());
    }
    // What the app's MCP server was sent: the calls' arguments, never a key.
    for request in setup.server.providers.requests.lock().unwrap().iter() {
        seen.push(String::from_utf8_lossy(&request.body).into_owned());
    }
    let logs = logs.text();
    assert!(logs.contains("runs.model_answered"), "the logs were captured");
    seen.push(logs);
    for key in &keys {
        for shown in &seen {
            assert!(!shown.contains(key.as_str()), "{key} surfaced in {shown}");
        }
    }
    assert!(
        setup
            .server
            .models
            .ledger()
            .iter()
            .all(|received| !received.key_elsewhere),
        "sent only as the credential"
    );
}

#[tokio::test]
async fn fails_a_run_whose_key_the_provider_refuses_mid_run() {
    let setup = setup().await;
    let (_, connection) = setup.default_route().await;
    let key = setup.api_key().await;
    // The first turn asks for a call that needs a person's approval.
    let run = setup.start(&setup.calling("send", "{}").await).await;
    setup.work().await;
    assert_eq!(setup.get(run).await["status"], "waiting_for_approval");
    let turns_before = setup.server.models.turns().len();
    // Meanwhile, the key is revoked at the provider.
    setup.server.models.revoke(PROVIDER_KEY);
    let call_id = setup.calls(run).await[0].id.to_string();
    let (status, body) = setup.decide(&key, run, &call_id, "approved").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    setup.work().await;
    let failed = setup.get(run).await;
    assert_eq!(failed["status"], "failed", "{failed}");
    assert_eq!(error_code(&failed), "model_key_invalid");
    assert_eq!(
        failed["error"]["message"],
        "The model provider refused OpenAI's key. Replace it under Model providers."
    );
    assert_eq!(
        setup.server.models.turns().len(),
        turns_before + 1,
        "a refused key is asked once, never again"
    );
    assert_eq!(setup.connection(&connection).await["status"], "invalid_key");

    // A new run fails at once, without asking the provider.
    let next = setup.start("hello").await;
    setup.work().await;
    assert_eq!(error_code(&setup.get(next).await), "model_key_invalid");
    assert_eq!(setup.server.models.turns().len(), turns_before + 1);

    // A new key makes the connection active, and runs go through with it.
    let fresh = "sk-test-fresh-0123456789abcdef";
    let (status, replaced) = setup
        .kit
        .send(
            call(
                "alice",
                Method::PUT,
                format!("/v1/organizations/{}/model-connections/{connection}/api-key", setup.org),
            )
            .body(json!({ "api_key": fresh })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    assert_eq!(replaced["data"]["status"], "active");
    let again = setup.start("hello").await;
    setup.work().await;
    assert_eq!(setup.get(again).await["status"], "completed");
    let last = setup.server.models.turns().pop().unwrap();
    assert_eq!(
        last.credential,
        Some(("Authorization".to_owned(), format!("Bearer {fresh}"))),
        "the replacement, opened at its own version"
    );
    assert_eq!(setup.attempts(again).await, [(Some(2), "answered".to_owned())]);
}

#[tokio::test]
async fn marks_a_model_removed_upstream_unavailable_and_fails_the_run() {
    let setup = setup().await;
    let (model, _) = setup.default_route().await;
    setup.server.models.remove_model(ProviderKind::OpenAi, DEFAULT_MODEL);
    let run = setup.start("hello").await;
    setup.work().await;
    let failed = setup.get(run).await;
    assert_eq!(error_code(&failed), "model_unavailable", "{failed}");
    assert_eq!(
        failed["error"]["message"],
        format!("The model {DEFAULT_MODEL} isn't available. Choose another model for the agent and start a new run.")
    );
    assert_eq!(setup.server.models.turns().len(), 1, "a 404 isn't tried again");
    assert_eq!(setup.model(&model).await["status"], "unavailable");

    // Served again: the next run's answer makes it available.
    setup.server.models.restore_model(ProviderKind::OpenAi, DEFAULT_MODEL);
    let next = setup.start("hello").await;
    setup.work().await;
    assert_eq!(setup.get(next).await["status"], "completed");
    assert_eq!(setup.model(&model).await["status"], "available");
}

#[tokio::test]
async fn tries_a_rate_limited_or_failing_turn_again_through_the_jobs_attempts() {
    for faults in [
        vec![Fault::Status(429)],
        vec![Fault::Status(503), Fault::TimedOut],
        vec![Fault::Status(500), Fault::Unreachable, Fault::Status(529)],
        vec![
            Fault::RetryAfter { status: 429, secs: 45 },
            Fault::RetryAfter {
                status: 503,
                secs: 3600,
            },
        ],
    ] {
        let setup = setup().await;
        setup.server.models.fail_next(&faults);
        let run = setup.start("hello").await;
        setup.work().await;
        let finished = setup.get(run).await;
        assert_eq!(finished["status"], "completed", "{faults:?}: {finished}");
        assert_eq!(finished["output"], "You said: hello");
        let attempts = setup.attempts(run).await;
        assert_eq!(attempts.len(), faults.len() + 1, "{faults:?}");
        assert!(
            attempts[..faults.len()].iter().all(|(_, state)| state == "unanswered"),
            "{attempts:?}"
        );
        assert_eq!(setup.server.models.turns().len(), faults.len() + 1);
    }
}

#[tokio::test]
async fn fails_a_run_once_the_provider_fails_every_attempt() {
    let setup = setup().await;
    let attempts = usize::try_from(RUN_JOB_ATTEMPTS).unwrap();
    setup.server.models.fail_next(&vec![Fault::Status(500); attempts + 3]);
    let run = setup.start("hello").await;
    setup.work().await;
    let failed = setup.get(run).await;
    assert_eq!(failed["status"], "failed", "{failed}");
    assert_eq!(error_code(&failed), "model_unavailable");
    assert_eq!(
        failed["error"]["message"],
        "The model provider did not respond in time."
    );
    assert_eq!(
        setup.server.models.turns().len(),
        attempts,
        "one turn per job attempt, and no more"
    );
    // The model and the connection are as they were: nothing was refused.
    let (model, connection) = setup.default_route().await;
    assert_eq!(setup.model(&model).await["status"], "available");
    assert_eq!(setup.connection(&connection).await["status"], "active");
}

#[tokio::test]
async fn tries_a_timed_out_turn_again_once_since_each_may_be_billed() {
    let setup = setup().await;
    setup.server.models.fail_next(&[Fault::TimedOut; 4]);
    let run = setup.start("hello").await;
    setup.work().await;
    let failed = setup.get(run).await;
    assert_eq!(error_code(&failed), "model_failed", "{failed}");
    assert!(
        failed["error"]["message"]
            .as_str()
            .unwrap()
            .contains("took too long to answer twice"),
        "{failed}"
    );
    assert_eq!(setup.server.models.turns().len(), 2, "tried again once, not more");
    assert_eq!(
        setup.attempts(run).await,
        [(Some(1), "unanswered".to_owned()), (Some(1), "abandoned".to_owned())]
    );
}

#[tokio::test]
async fn fails_a_turn_the_provider_refuses_without_trying_again() {
    let setup = setup().await;
    setup.server.models.fail_next(&[Fault::Status(400)]);
    let run = setup.start("hello").await;
    setup.work().await;
    let failed = setup.get(run).await;
    assert_eq!(error_code(&failed), "model_failed", "{failed}");
    assert_eq!(setup.server.models.turns().len(), 1);
}

#[tokio::test]
async fn fails_a_turn_refused_as_forbidden_or_unpaid_without_marking_the_key() {
    for (fault, says) in [
        (403, "OpenRouter refused this request with OpenRouter's key"),
        (402, "The OpenRouter account behind OpenRouter's key is out of credits"),
    ] {
        let setup = setup().await;
        let (connection, model) = setup
            .route(
                ProviderKind::OpenRouter,
                "sk-or-0123456789abcdefWXYZ",
                served(ProviderKind::OpenRouter),
            )
            .await;
        setup.server.models.fail_next(&[Fault::Status(fault)]);
        let run = setup.start("hello").await;
        setup.work().await;
        let failed = setup.get(run).await;
        assert_eq!(error_code(&failed), "model_failed", "{fault}: {failed}");
        assert!(
            failed["error"]["message"].as_str().unwrap().starts_with(says),
            "{failed}"
        );
        assert_eq!(setup.server.models.turns().len(), 1, "{fault} isn't tried again");
        // The key and the model are as they were: the next run goes through.
        assert_eq!(setup.connection(&connection).await["status"], "active");
        assert_eq!(setup.model(&model).await["status"], "available");
        let next = setup.start("hello").await;
        setup.work().await;
        assert_eq!(setup.get(next).await["status"], "completed", "{fault}");
    }
}

#[tokio::test]
async fn marks_a_model_openrouter_no_longer_knows_unavailable() {
    let setup = setup().await;
    let routed = served(ProviderKind::OpenRouter);
    let (connection, model) = setup
        .route(ProviderKind::OpenRouter, "sk-or-0123456789abcdefWXYZ", routed)
        .await;
    setup.server.models.remove_model(ProviderKind::OpenRouter, routed);
    let run = setup.start("hello").await;
    setup.work().await;
    let failed = setup.get(run).await;
    assert_eq!(error_code(&failed), "model_unavailable", "{failed}");
    let turn = setup.server.models.turns().pop().unwrap();
    assert_eq!(turn.status, Some(400), "OpenRouter's way of saying it");
    assert_eq!(setup.model(&model).await["status"], "unavailable");
    assert_eq!(setup.connection(&connection).await["status"], "active");
}

#[tokio::test]
async fn asks_again_with_a_key_replaced_while_a_turn_was_in_flight() {
    let setup = setup().await;
    let (_, connection) = setup.default_route().await;
    // While the first turn is in flight, the key is replaced and the old
    // one revoked: the provider refuses the turn with the old key.
    let app = setup.server.app.clone();
    let org = setup.org.clone();
    let fresh = "sk-test-fresh-0123456789abcdef";
    let path = format!("/v1/organizations/{org}/model-connections/{connection}/api-key");
    let models = setup.server.models.clone();
    setup.server.models.before_next_turn(async move {
        let (status, body) = common::send_to(
            &app,
            call("alice", Method::PUT, path)
                .naming(&org)
                .body(json!({ "api_key": fresh })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        models.revoke(PROVIDER_KEY);
    });
    let run = setup.start("hello").await;
    setup.work().await;
    let finished = setup.get(run).await;
    assert_eq!(finished["status"], "completed", "{finished}");
    assert_eq!(
        setup.attempts(run).await,
        [(Some(1), "unanswered".to_owned()), (Some(2), "answered".to_owned())],
        "the refused attempt was the old key's; the next took the new one"
    );
    assert_eq!(setup.connection(&connection).await["status"], "active", "not marked");
}

#[tokio::test]
async fn opens_a_key_only_at_the_version_and_connection_the_attempt_names() {
    use zupflow_core::environment::Environment;
    use zupflow_core::ids::{ModelConnectionId, OrganizationId};
    use zupflow_server::model_access::{self, Unopened};

    let setup = setup().await;
    let (_, connection) = setup.default_route().await;
    let org: OrganizationId = setup.org.parse().unwrap();
    let connection: ModelConnectionId = connection.parse().unwrap();
    let state = &setup.server.state;
    let opened = model_access::open(state, org, Environment::Sandbox, connection, 1)
        .await
        .ok()
        .unwrap();
    assert_eq!(opened.key.expose(), PROVIDER_KEY);
    assert_eq!(
        model_access::open(state, org, Environment::Sandbox, connection, 2)
            .await
            .err(),
        Some(Unopened::KeyReplaced),
        "a version that isn't the current one"
    );
    assert_eq!(
        model_access::open(state, org, Environment::Production, connection, 1)
            .await
            .err(),
        Some(Unopened::ConnectionGone),
        "never through another environment"
    );
    assert_eq!(
        model_access::open(state, org, Environment::Sandbox, ModelConnectionId::generate(), 1)
            .await
            .err(),
        Some(Unopened::ConnectionGone)
    );
    let other: OrganizationId = setup.server.organization("bob", "elsewhere").await.parse().unwrap();
    assert_eq!(
        model_access::open(state, other, Environment::Sandbox, connection, 1)
            .await
            .err(),
        Some(Unopened::ConnectionGone),
        "never through another organization"
    );
}

#[tokio::test]
async fn checks_keys_and_lists_models_with_each_provider() {
    let setup = setup().await;
    // Each provider refuses a key it refuses: the connection isn't made.
    for kind in ProviderKind::ALL {
        let (status, refused) = setup
            .kit
            .send(
                call("alice", Method::POST, format!("/v1/organizations/{}/model-connections", setup.org)).body(json!({
                    "environment": "sandbox", "kind": kind.as_str(), "label": "Refused", "api_key": "sk-test-invalid-0123456789abcdef",
                })),
            )
            .await;
        assert_eq!(
            (status, common::code(&refused)),
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid_api_key"),
            "{kind:?}"
        );
    }
    // A revoked key is refused as the provider refuses it.
    let revoked = "sk-test-gone-0123456789abcdef";
    setup.server.models.revoke(revoked);
    let (status, refused) = setup
        .kit
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/organizations/{}/model-connections", setup.org),
            )
            .body(json!({
                "environment": "sandbox", "kind": "anthropic", "label": "Gone", "api_key": revoked,
            })),
        )
        .await;
    assert_eq!(
        (status, common::code(&refused)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_api_key")
    );
    // OpenRouter's list, as its models API describes it, through the adapter.
    let (connection, _) = setup
        .route(
            ProviderKind::OpenRouter,
            "sk-or-0123456789abcdefWXYZ",
            "openai/gpt-5-mini",
        )
        .await;
    let listed = setup
        .read(format!(
            "/v1/organizations/{}/model-connections/{connection}/provider-models?q=gpt-5",
            setup.org
        ))
        .await;
    let gpt5 = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["provider_model_id"] == "openai/gpt-5")
        .unwrap();
    assert_eq!(gpt5["name"], "GPT-5");
    assert_eq!(gpt5["pricing"]["input_per_million_usd"], json!(1.25));
    for received in setup
        .server
        .models
        .ledger()
        .iter()
        .filter(|received| !received.is_turn())
    {
        assert_eq!(received.limits, zupflow_providers::LISTING_LIMITS);
    }
}
