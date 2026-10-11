//! Webhooks through the API and the delivery worker (decision 0017):
//! endpoints managed by owners, admins and API keys of their environment; a
//! secret shown once; a test button that sends a real signed event; and run
//! and tool events delivered, signed, to the subscribed endpoints of their
//! own organization and environment only, retried until they succeed or run
//! out of attempts. And the usage and workspace stats runs add up to.

#![allow(clippy::unwrap_used)]

mod common;

use axum::http::{Method, StatusCode};
use common::{Recorded, Server, call, code};
use serde_json::{Value, json};
use zupflow_core::outbound::OutboundError;
use zupflow_core::webhooks::{DELIVERY_ATTEMPTS, signature};
use zupflow_server::{discovery, run_engine, webhook_delivery};

const MCP: &str = "https://tools.example/mcp";

#[path = "webhooks/rotation.rs"]
mod rotation;
const HOOK: &str = "https://hooks.example/sandbox";
const PRODUCTION_HOOK: &str = "https://hooks.example/production";
const THEIR_HOOK: &str = "https://hooks.example/theirs";

fn id(value: &Value) -> &str {
    value["id"].as_str().unwrap()
}

async fn api_key(server: &Server, owner: &str, org: &str, environment: &str) -> String {
    let (status, body) = server
        .send(
            call(owner, Method::POST, format!("/v1/organizations/{org}/api-keys"))
                .body(json!({ "name": "Backend", "environment": environment })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["data"]["secret"].as_str().unwrap().to_owned()
}

/// Adds an endpoint; the endpoint and its secret.
async fn hook(server: &Server, who: &str, org: &str, environment: &str, url: &str, events: Value) -> (Value, String) {
    let (status, body) = server
        .send(
            call(who, Method::POST, format!("/v1/organizations/{org}/webhooks")).body(json!({
                "environment": environment, "url": url, "description": "Runs", "events": events,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    (
        body["data"]["endpoint"].clone(),
        body["data"]["signing_secret"].as_str().unwrap().to_owned(),
    )
}

/// Checks a delivery's signature against the secret, as `verifyWebhook` does.
fn verify(request: &Recorded, secret: &str) {
    let header = request.header("Zupflow-Signature").unwrap();
    let t: i64 = header
        .strip_prefix("t=")
        .and_then(|rest| rest.split(',').next())
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        header,
        signature(secret.as_bytes(), t, &request.body),
        "signed with the endpoint's secret"
    );
    assert!(
        (t - zupflow_core::time::Timestamp::now().millis() / 1000).abs() < 60,
        "timestamped now"
    );
    assert_eq!(request.header("User-Agent"), Some("Zupflow-Webhooks/1"));
    assert_eq!(request.header("Content-Type"), Some("application/json"));
}

async fn work(server: &Server) {
    loop {
        let ran = run_engine::work_once(&server.state).await;
        let delivered = webhook_delivery::work_once(&server.state).await;
        let discovered = discovery::work_once(&server.state).await;
        if !ran && !delivered && !discovered {
            break;
        }
    }
}

async fn agent(server: &Server, who: &str, org: &str, ws: &str) -> String {
    let (status, agent) = server
        .send(
            call(who, Method::POST, format!("/v1/workspaces/{ws}/agents"))
                .naming(org)
                .body(json!({ "name": "Helper", "description": "", "model": "model-a", "instructions": "Be brief." })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{agent}");
    id(&agent["data"]).to_owned()
}

async fn start(server: &Server, who: &str, org: &str, ws: &str, agent: &str, input: &str) -> String {
    let (status, run) = server
        .send(
            call(who, Method::POST, format!("/v1/workspaces/{ws}/agents/{agent}/runs"))
                .naming(org)
                .body(json!({ "input": input })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{run}");
    id(&run["data"]).to_owned()
}

/// Opens the test server's database, for what the API can't reach: moving
/// a retry's time forward.
async fn pool(server: &Server) -> sqlx::SqlitePool {
    sqlx::SqlitePool::connect(&format!("sqlite:{}", server.directory.join("zupflow.sqlite").display()))
        .await
        .unwrap()
}

#[tokio::test]
async fn manages_endpoints_by_role_and_environment() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    server.member("alice", &org, "dev", "developer").await;
    server.member("alice", &org, "val", "viewer").await;

    let (endpoint, secret) = hook(
        &server,
        "alice",
        &org,
        "sandbox",
        HOOK,
        json!(["run.completed", "run.completed"]),
    )
    .await;
    assert!(secret.starts_with("whsec_test_"), "{secret}");
    assert_eq!(endpoint["signing_secret_last4"], secret[secret.len() - 4..]);
    assert_eq!(endpoint["events"], json!(["run.completed"]), "each event once");
    assert_eq!(
        (endpoint["enabled"].clone(), endpoint["last_delivery"].clone()),
        (json!(true), Value::Null)
    );
    let hook_id = id(&endpoint).to_owned();

    // The secret is shown once: never listed, never read again.
    for who in ["alice", "dev", "val"] {
        let (status, list) = server
            .send(call(who, Method::GET, format!("/v1/organizations/{org}/webhooks")))
            .await;
        assert_eq!(status, StatusCode::OK, "every member lists them");
        assert_eq!(list["data"].as_array().unwrap().len(), 1);
        assert!(!list.to_string().contains(&secret), "the secret never shows again");
    }

    // Admins and owners change them; developers and viewers don't.
    for who in ["dev", "val"] {
        let (status, body) = server
            .send(
                call(who, Method::POST, format!("/v1/organizations/{org}/webhooks")).body(
                    json!({ "environment": "sandbox", "url": HOOK, "description": "", "events": ["run.failed"] }),
                ),
            )
            .await;
        assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, "forbidden"), "{body}");
        for (method, path) in [
            (Method::PATCH, format!("/v1/organizations/{org}/webhooks/{hook_id}")),
            (Method::DELETE, format!("/v1/organizations/{org}/webhooks/{hook_id}")),
            (Method::POST, format!("/v1/organizations/{org}/webhooks/{hook_id}/test")),
        ] {
            let (status, body) = server
                .send(call(who, method, path).body(json!({ "enabled": false })))
                .await;
            assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, "forbidden"), "{body}");
        }
    }

    // What an endpoint is checked for.
    for (body, status, error) in [
        (
            json!({ "environment": "sandbox", "url": "http://hooks.example/x", "description": "", "events": ["run.failed"] }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "insecure_url",
        ),
        (
            json!({ "environment": "sandbox", "url": "https://10.0.0.1/x", "description": "", "events": ["run.failed"] }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "destination_not_allowed",
        ),
        (
            json!({ "environment": "sandbox", "url": HOOK, "description": "", "events": [] }),
            StatusCode::BAD_REQUEST,
            "validation_failed",
        ),
        (
            json!({ "environment": "sandbox", "url": HOOK, "description": "", "events": ["webhook.test"] }),
            StatusCode::BAD_REQUEST,
            "validation_failed",
        ),
        (
            json!({ "environment": "staging", "url": HOOK, "description": "", "events": ["run.failed"] }),
            StatusCode::BAD_REQUEST,
            "validation_failed",
        ),
        (
            json!({ "environment": "sandbox", "url": HOOK, "description": "x".repeat(201), "events": ["run.failed"] }),
            StatusCode::BAD_REQUEST,
            "validation_failed",
        ),
    ] {
        let (got, answer) = server
            .send(call("alice", Method::POST, format!("/v1/organizations/{org}/webhooks")).body(body))
            .await;
        assert_eq!((got, code(&answer)), (status, error), "{answer}");
    }
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::PATCH,
                format!("/v1/organizations/{org}/webhooks/{hook_id}"),
            )
            .body(json!({ "url": "" })),
        )
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::BAD_REQUEST, "validation_failed"),
        "an empty URL isn't one"
    );
    assert!(body["error"]["fields"]["url"].is_string());

    let (status, updated) = server
        .send(
            call(
                "alice",
                Method::PATCH,
                format!("/v1/organizations/{org}/webhooks/{hook_id}"),
            )
            .body(json!({ "events": ["run.failed", "app.tools_changed"], "enabled": false })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["data"]["events"], json!(["run.failed", "app.tools_changed"]));
    assert_eq!(
        (updated["data"]["enabled"].clone(), updated["data"]["url"].clone()),
        (json!(false), json!(HOOK))
    );

    // An API key reaches its own environment's endpoints only.
    let production_key = api_key(&server, "alice", &org, "production").await;
    let (status, list) = server
        .send(call(
            &production_key,
            Method::GET,
            format!("/v1/organizations/{org}/webhooks"),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["data"], json!([]), "a Production key lists no Sandbox endpoint");
    for (method, path) in [
        (Method::PATCH, format!("/v1/organizations/{org}/webhooks/{hook_id}")),
        (Method::POST, format!("/v1/organizations/{org}/webhooks/{hook_id}/test")),
        (Method::DELETE, format!("/v1/organizations/{org}/webhooks/{hook_id}")),
    ] {
        let (status, body) = server
            .send(call(&production_key, method, path).body(json!({ "enabled": true })))
            .await;
        assert_eq!((status, code(&body)), (StatusCode::NOT_FOUND, "not_found"), "{body}");
    }
    let (status, body) = server
        .send(
            call(
                &production_key,
                Method::POST,
                format!("/v1/organizations/{org}/webhooks"),
            )
            .body(json!({ "environment": "sandbox", "url": HOOK, "description": "", "events": ["run.failed"] })),
        )
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "environment_mismatch")
    );
    let (endpoint, secret) = hook(
        &server,
        &production_key,
        &org,
        "production",
        PRODUCTION_HOOK,
        json!(["run.completed"]),
    )
    .await;
    assert!(secret.starts_with("whsec_live_"));
    let (status, list) = server
        .send(call(
            &production_key,
            Method::GET,
            format!("/v1/organizations/{org}/webhooks"),
        ))
        .await;
    assert_eq!((status, list["data"].as_array().unwrap().len()), (StatusCode::OK, 1));

    // Another organization's owner, and an endpoint ID through the wrong organization, find nothing.
    let theirs = server.organization("mallory", "larkspur").await;
    for (who, organization) in [("mallory", org.as_str()), ("mallory", theirs.as_str())] {
        let (status, body) = server
            .send(call(
                who,
                Method::DELETE,
                format!("/v1/organizations/{organization}/webhooks/{}", id(&endpoint)),
            ))
            .await;
        assert_eq!((status, code(&body)), (StatusCode::NOT_FOUND, "not_found"), "{body}");
    }

    let (status, _) = server
        .send(call(
            "alice",
            Method::DELETE,
            format!("/v1/organizations/{org}/webhooks/{hook_id}"),
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = server
        .send(call(
            "alice",
            Method::DELETE,
            format!("/v1/organizations/{org}/webhooks/{hook_id}"),
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn sends_a_signed_test_event_and_answers_what_came_back() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let (endpoint, secret) = hook(&server, "alice", &org, "sandbox", HOOK, json!(["run.completed"])).await;
    let path = format!("/v1/organizations/{org}/webhooks/{}/test", id(&endpoint));

    let (status, delivery) = server.send(call("alice", Method::POST, path.clone())).await;
    assert_eq!(status, StatusCode::OK, "{delivery}");
    assert_eq!(delivery["data"]["status_code"], 200);
    let sent = server.providers.requests_to(HOOK);
    assert_eq!(sent.len(), 1);
    verify(&sent[0], &secret);
    let event: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(event["type"], "webhook.test");
    assert_eq!(event["workspace"], Value::Null);
    assert_eq!(event["environment"], "sandbox");
    assert_eq!(event["organization_id"], org.as_str());
    assert_eq!(
        event["data"],
        json!({ "object": "webhook_endpoint", "id": id(&endpoint) })
    );
    assert_eq!(sent[0].header("Zupflow-Event-Id"), event["id"].as_str());
    assert_eq!(sent[0].header("Zupflow-Event-Type"), Some("webhook.test"));
    assert_eq!(sent[0].header("Zupflow-Delivery-Attempt"), Some("1"));

    let (_, list) = server
        .send(call("alice", Method::GET, format!("/v1/organizations/{org}/webhooks")))
        .await;
    assert_eq!(
        list["data"][0]["last_delivery"], delivery["data"],
        "the endpoint's last delivery"
    );

    server.providers.answer(Ok(503));
    let (status, delivery) = server.send(call("alice", Method::POST, path.clone())).await;
    assert_eq!(
        (status, delivery["data"]["status_code"].clone()),
        (StatusCode::OK, json!(503))
    );
    server.providers.answer(Err(OutboundError::TimedOut));
    let (status, delivery) = server.send(call("alice", Method::POST, path)).await;
    assert_eq!(
        (status, delivery["data"]["status_code"].clone()),
        (StatusCode::OK, json!(0)),
        "no answer is 0"
    );
    // A test is no event anyone subscribes to: nothing is queued.
    assert!(!webhook_delivery::work_once(&server.state).await);

    let history_path = format!("/v1/organizations/{org}/webhooks/{}/deliveries", id(&endpoint));
    let (status, history) = server.send(call("alice", Method::GET, &history_path)).await;
    assert_eq!(status, StatusCode::OK, "{history}");
    assert_eq!(history["meta"]["pagination"]["total"], 3);
    let timeout = &history["data"][0];
    assert_eq!(timeout["state"], "failed");
    assert_eq!(timeout["last_status_code"], Value::Null);
    assert_eq!(timeout["last_error"], "timeout");
    assert_eq!(timeout["next_attempt_at"], Value::Null);
    assert_eq!(timeout["workspace"], Value::Null);
    let (status, detail) = server
        .send(call("alice", Method::GET, format!("{history_path}/{}", id(timeout))))
        .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["data"]["attempt_history_complete"], true);
    let sent = server.providers.requests_to(HOOK);
    let last_sent = sent.last().unwrap();
    let last_event: Value = serde_json::from_slice(&last_sent.body).unwrap();
    assert_eq!(
        detail["data"]["event"], last_event,
        "the exact lean event that was sent"
    );
    let (status, attempts) = server
        .send(call(
            "alice",
            Method::GET,
            format!("{history_path}/{}/attempts", id(timeout)),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{attempts}");
    assert_eq!(attempts["meta"]["pagination"]["total"], 1);
    assert_eq!(attempts["data"][0]["number"], 1);
    assert_eq!(attempts["data"][0]["error_class"], "timeout");
    assert!(attempts["data"][0]["duration_ms"].as_u64().is_some());
    for response in [&history, &detail, &attempts] {
        assert!(!response.to_string().contains(&secret));
        assert!(
            !response
                .to_string()
                .contains(last_sent.header("Zupflow-Signature").unwrap())
        );
    }
}

/// Two organizations, each with Sandbox endpoints; Northwind also has a
/// Production one and a disabled one. A Sandbox run's events reach only
/// Northwind's enabled Sandbox endpoints that subscribe to them.
#[tokio::test]
async fn delivers_run_events_to_their_own_organization_and_environment() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let sandbox = server.workspace("alice", &org, "acme-sandbox", "sandbox").await;
    let production = server.workspace("alice", &org, "acme", "production").await;
    let (sandbox_hook, secret) = hook(
        &server,
        "alice",
        &org,
        "sandbox",
        HOOK,
        json!(["run.completed", "run.cancelled"]),
    )
    .await;
    hook(
        &server,
        "alice",
        &org,
        "production",
        PRODUCTION_HOOK,
        json!(["run.completed"]),
    )
    .await;
    let (disabled, _) = hook(
        &server,
        "alice",
        &org,
        "sandbox",
        "https://hooks.example/off",
        json!(["run.completed"]),
    )
    .await;
    let (status, _) = server
        .send(
            call(
                "alice",
                Method::PATCH,
                format!("/v1/organizations/{org}/webhooks/{}", id(&disabled)),
            )
            .body(json!({ "enabled": false })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let theirs = server.organization("mallory", "larkspur").await;
    let their_sandbox = server.workspace("mallory", &theirs, "harbor-sandbox", "sandbox").await;
    hook(
        &server,
        "mallory",
        &theirs,
        "sandbox",
        THEIR_HOOK,
        json!(["run.completed"]),
    )
    .await;

    let ws = id(&sandbox).to_owned();
    let helper = agent(&server, "alice", &org, &ws).await;
    let run = start(&server, "alice", &org, &ws, &helper, "Hello").await;
    work(&server).await;

    let sent = server.providers.requests_to(HOOK);
    assert_eq!(sent.len(), 1, "one delivery of the run's completion");
    verify(&sent[0], &secret);
    let event: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert!(event["id"].as_str().unwrap().starts_with("evt_"));
    assert_eq!(event["type"], "run.completed");
    assert_eq!(event["environment"], "sandbox");
    assert_eq!(event["organization_id"], org.as_str());
    assert_eq!(event["workspace"], json!({ "id": ws, "external_id": null }));
    assert_eq!(
        event["data"],
        json!({ "object": "run", "id": run, "agent_id": helper, "status": "completed" }),
        "lean: what changed and where, never content"
    );
    assert!(
        !sent[0].body.windows(5).any(|window| window == b"Hello"),
        "never the run's input"
    );
    assert_eq!(sent[0].header("Zupflow-Event-Id"), event["id"].as_str());
    assert!(
        server.providers.requests_to(PRODUCTION_HOOK).is_empty(),
        "never another environment's endpoint"
    );
    assert!(
        server.providers.requests_to(THEIR_HOOK).is_empty(),
        "never another organization's"
    );
    assert!(
        server.providers.requests_to("https://hooks.example/off").is_empty(),
        "never a disabled one"
    );

    let (_, list) = server
        .send(call("alice", Method::GET, format!("/v1/organizations/{org}/webhooks")))
        .await;
    let delivered = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|endpoint| endpoint["id"] == sandbox_hook["id"])
        .unwrap();
    assert_eq!(delivered["last_delivery"]["status_code"], 200);

    // A cancelled run's event, and nothing for one the subscriber doesn't take.
    let queued = start(&server, "alice", &org, &ws, &helper, "Wait").await;
    let (status, _) = server
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/workspaces/{ws}/runs/{queued}/cancel"),
            )
            .naming(&org),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    work(&server).await;
    let types: Vec<String> = server
        .providers
        .requests_to(HOOK)
        .iter()
        .map(|request| request.header("Zupflow-Event-Type").unwrap().to_owned())
        .collect();
    assert_eq!(types, ["run.completed", "run.cancelled"]);

    // Production's run reaches Production's endpoint only, and Larkspur's run Larkspur's.
    let key = api_key(&server, "alice", &org, "production").await;
    let production_agent = agent(&server, "alice", &org, id(&production)).await;
    start(&server, &key, &org, id(&production), &production_agent, "Hi").await;
    let their_agent = agent(&server, "mallory", &theirs, id(&their_sandbox)).await;
    start(&server, "mallory", &theirs, id(&their_sandbox), &their_agent, "Hi").await;
    work(&server).await;
    assert_eq!(server.providers.requests_to(PRODUCTION_HOOK).len(), 1);
    assert_eq!(server.providers.requests_to(THEIR_HOOK).len(), 1);
    assert_eq!(
        server.providers.requests_to(HOOK).len(),
        2,
        "nothing more to Northwind's Sandbox"
    );
    let their_event: Value = serde_json::from_slice(&server.providers.requests_to(THEIR_HOOK)[0].body).unwrap();
    assert_eq!(their_event["organization_id"], theirs.as_str());
}

/// A failing endpoint is tried again, later, with the same event, until it
/// runs out of attempts; a success stops the retries.
#[tokio::test]
async fn retries_a_failing_delivery_until_its_attempts_run_out() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let sandbox = server.workspace("alice", &org, "acme-sandbox", "sandbox").await;
    let (endpoint, _) = hook(&server, "alice", &org, "sandbox", HOOK, json!(["run.completed"])).await;
    let ws = id(&sandbox).to_owned();
    let helper = agent(&server, "alice", &org, &ws).await;
    server.providers.script(HOOK, 500, json!({ "error": "down" }));
    start(&server, "alice", &org, &ws, &helper, "Hello").await;
    work(&server).await;
    assert_eq!(server.providers.requests_to(HOOK).len(), 1, "the first attempt failed");
    assert!(
        !webhook_delivery::work_once(&server.state).await,
        "the next waits its turn"
    );

    let pool = pool(&server).await;
    let next: i64 = sqlx::query_scalar("SELECT next_attempt_at - last_attempt_at FROM webhook_deliveries")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!((54_000..=66_000).contains(&next), "about a minute later: {next}");
    for _ in 1..DELIVERY_ATTEMPTS {
        sqlx::query("UPDATE jobs SET run_at = 0 WHERE kind = 'deliver_webhook' AND state = 'queued'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(webhook_delivery::work_once(&server.state).await);
    }
    let sent = server.providers.requests_to(HOOK);
    assert_eq!(sent.len(), 8, "eight attempts");
    let attempts: Vec<&str> = sent
        .iter()
        .map(|request| request.header("Zupflow-Delivery-Attempt").unwrap())
        .collect();
    assert_eq!(attempts, ["1", "2", "3", "4", "5", "6", "7", "8"]);
    assert!(
        sent.iter()
            .all(|request| request.header("Zupflow-Event-Id") == sent[0].header("Zupflow-Event-Id")),
        "the same event every time"
    );
    let (state, attempts): (String, i64) = sqlx::query_as("SELECT state, attempts FROM webhook_deliveries")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((state.as_str(), attempts), ("failed", 8));
    let path = format!("/v1/organizations/{org}/webhooks/{}/deliveries", id(&endpoint));
    let (status, history) = server.send(call("alice", Method::GET, &path)).await;
    assert_eq!(status, StatusCode::OK, "{history}");
    let delivery = id(&history["data"][0]);
    let (_, detail) = server
        .send(call("alice", Method::GET, format!("{path}/{delivery}")))
        .await;
    assert_eq!(detail["data"]["attempts"], 8);
    assert_eq!(detail["data"]["attempt_history_complete"], true);
    assert_eq!(detail["data"]["next_attempt_at"], Value::Null);
    let (_, history_attempts) = server
        .send(call("alice", Method::GET, format!("{path}/{delivery}/attempts")))
        .await;
    assert_eq!(history_attempts["meta"]["pagination"]["total"], 8);
    for (index, attempt) in history_attempts["data"].as_array().unwrap().iter().enumerate() {
        assert_eq!(attempt["number"], index + 1);
        assert_eq!(attempt["status_code"], 500);
        assert_eq!(attempt["error_class"], "status");
        assert!(attempt["duration_ms"].as_u64().is_some());
    }
    assert!(
        !detail.to_string().contains("down"),
        "receiver response bodies never enter history"
    );
    sqlx::query("UPDATE jobs SET run_at = 0 WHERE kind = 'deliver_webhook'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        !webhook_delivery::work_once(&server.state).await,
        "nothing after the last"
    );

    // A manual resend has a fresh, bounded delivery budget. It does not
    // revive the exhausted source or change any of its attempts.
    let (status, resent) = server
        .send(call("alice", Method::POST, format!("{path}/{delivery}/resend")))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{resent}");
    let new_delivery = id(&resent["data"]);
    assert_ne!(new_delivery, delivery);
    assert_eq!(resent["data"]["state"], "pending");
    assert_eq!(resent["data"]["attempts"], 0);
    assert_eq!(resent["data"]["next_attempt_at"], resent["data"]["created_at"]);
    let org_id = zupflow_core::ids::OrganizationId::parse(&org).unwrap();
    let new_id = zupflow_core::ids::WebhookDeliveryId::parse(new_delivery).unwrap();
    let payload = zupflow_core::webhooks::delivery_job(new_id).unwrap().payload;
    let budget: (i64, i64) = sqlx::query_as("SELECT attempts, max_attempts FROM jobs WHERE organization_id = ? AND kind = 'deliver_webhook' AND payload = ?")
        .bind(org_id.uuid()).bind(&payload).fetch_one(&pool).await.unwrap();
    assert_eq!(budget, (0, i64::from(zupflow_core::webhooks::DELIVERY_JOB_ATTEMPTS)));
    for number in 1..=DELIVERY_ATTEMPTS {
        assert!(webhook_delivery::work_once(&server.state).await);
        let sent = server.providers.requests_to(HOOK);
        let request = sent.last().unwrap();
        assert_eq!(
            request.header("Zupflow-Delivery-Attempt"),
            Some(number.to_string().as_str())
        );
        assert_eq!(request.body, sent[0].body);
        assert_eq!(request.header("Zupflow-Event-Id"), sent[0].header("Zupflow-Event-Id"));
        assert!(
            !webhook_delivery::work_once(&server.state).await,
            "resend retries wait for their due time"
        );
        if number == 1 {
            let next: i64 = sqlx::query_scalar(
                "SELECT next_attempt_at - last_attempt_at FROM webhook_deliveries WHERE organization_id = ? AND id = ?",
            )
            .bind(org_id.uuid())
            .bind(new_id.uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(
                (54_000..=66_000).contains(&next),
                "resend starts the normal first backoff: {next}"
            );
        }
        sqlx::query("UPDATE jobs SET run_at = 0 WHERE organization_id = ? AND kind = 'deliver_webhook' AND payload = ? AND state = 'queued'")
            .bind(org_id.uuid()).bind(&payload).execute(&pool).await.unwrap();
    }
    assert!(
        !webhook_delivery::work_once(&server.state).await,
        "resend also stops after eight sends"
    );
    let (_, original_after) = server
        .send(call("alice", Method::GET, format!("{path}/{delivery}")))
        .await;
    let (_, original_attempts_after) = server
        .send(call("alice", Method::GET, format!("{path}/{delivery}/attempts")))
        .await;
    assert_eq!(original_after["data"], detail["data"]);
    assert_eq!(original_attempts_after["data"], history_attempts["data"]);
    let (_, retry_after) = server
        .send(call("alice", Method::GET, format!("{path}/{new_delivery}")))
        .await;
    assert_eq!(retry_after["data"]["state"], "failed");
    assert_eq!(retry_after["data"]["attempts"], 8);
    assert_eq!(retry_after["data"]["attempt_history_complete"], true);

    // A success stops the retries.
    server.providers.script(HOOK, 202, json!({}));
    start(&server, "alice", &org, &ws, &helper, "Again").await;
    work(&server).await;
    sqlx::query("UPDATE jobs SET run_at = 0 WHERE kind = 'deliver_webhook'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(!webhook_delivery::work_once(&server.state).await);
    assert_eq!(server.providers.requests_to(HOOK).len(), 17);
    pool.close().await;
}

/// A run waiting for approval, and a listing that changes a tool, reach the
/// endpoints that subscribe to them.
#[tokio::test]
async fn delivers_approvals_and_changed_tools() {
    let server = Server::new().await;
    let tools = |description: &str| {
        json!([
            { "name": "lookup", "description": "Look something up", "inputSchema": { "type": "object" }, "annotations": { "readOnlyHint": true } },
            { "name": "send", "description": description, "inputSchema": { "type": "object" } },
        ])
    };
    server.providers.serve_mcp(MCP, tools("Send it"));
    let org = server.organization("alice", "northwind").await;
    let sandbox = server.workspace("alice", &org, "acme-sandbox", "sandbox").await;
    let ws = id(&sandbox).to_owned();
    let (_, secret) = hook(
        &server,
        "alice",
        &org,
        "sandbox",
        HOOK,
        json!(["run.waiting_for_approval", "app.tools_changed"]),
    )
    .await;
    let (_, created) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/apps")).body(json!({
                "kind": "mcp", "name": "Tools", "slug": "tools", "description": "", "auth_type": "token",
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
    let (status, session) = server
        .send(
            call("alice", Method::POST, format!("/v1/workspaces/{ws}/connect-sessions"))
                .naming(&org)
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
    let installation = id(&done["data"]["installation"]).to_owned();
    work(&server).await;
    assert!(
        server.providers.requests_to(HOOK).is_empty(),
        "a first listing changes nothing"
    );

    let (status, body) = server
        .send(
            call(
                "alice",
                Method::PATCH,
                format!("/v1/organizations/{org}/apps/{app}/tool-levels"),
            )
            .body(json!({
                "levels": [{ "tool": "lookup", "level": "allow" }, { "tool": "send", "level": "approve" }],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let helper = agent(&server, "alice", &org, &ws).await;
    let (status, grant) = server
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/workspaces/{ws}/agents/{helper}/grants"),
            )
            .naming(&org)
            .body(
                json!({ "installation_id": installation, "tools": ["lookup", "send"], "approval_required": ["send"] }),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{grant}");
    let run = start(&server, "alice", &org, &ws, &helper, "call send {}").await;
    work(&server).await;
    let sent = server.providers.requests_to(HOOK);
    assert_eq!(sent.len(), 1, "{sent:?}");
    verify(&sent[0], &secret);
    let event: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(event["type"], "run.waiting_for_approval");
    assert_eq!(
        event["data"],
        json!({ "object": "run", "id": run, "agent_id": helper, "status": "waiting_for_approval" })
    );

    // The app's server changes a tool: the next listing announces it.
    server.providers.serve_mcp(MCP, tools("Send it to anyone"));
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/workspaces/{ws}/installations/{installation}/refresh-tools"),
            )
            .naming(&org),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    work(&server).await;
    let sent = server.providers.requests_to(HOOK);
    assert_eq!(sent.len(), 2);
    verify(&sent[1], &secret);
    let event: Value = serde_json::from_slice(&sent[1].body).unwrap();
    assert_eq!(event["type"], "app.tools_changed");
    assert_eq!(
        event["data"],
        json!({ "object": "installation", "id": installation, "app_id": app, "status": "connected" })
    );
    assert_eq!(event["workspace"]["id"], ws.as_str());

    // Changed again, but disconnected before the change is announced: no
    // event says a disconnected installation is connected.
    server.providers.serve_mcp(MCP, tools("Send it to no one"));
    let organization_id = zupflow_core::ids::OrganizationId::parse(&org).unwrap();
    let workspace_id = zupflow_core::ids::WorkspaceId::parse(&ws).unwrap();
    server
        .state
        .cache
        .delete_scope(&zupflow_core::scope::Scope::Workspace(organization_id, workspace_id))
        .await
        .unwrap();
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/workspaces/{ws}/installations/{installation}/refresh-tools"),
            )
            .naming(&org),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    while discovery::work_once(&server.state).await {}
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/workspaces/{ws}/installations/{installation}/disconnect"),
            )
            .naming(&org),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    work(&server).await;
    assert_eq!(server.providers.requests_to(HOOK).len(), 2, "nothing more");
}

#[tokio::test]
async fn counts_usage_and_recent_runs_from_runs() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    server.member("alice", &org, "val", "viewer").await;
    let sandbox = server.workspace("alice", &org, "acme-sandbox", "sandbox").await;
    let other = server.workspace("alice", &org, "beta-sandbox", "sandbox").await;
    let ws = id(&sandbox).to_owned();
    let helper = agent(&server, "alice", &org, &ws).await;
    for input in ["One", "Two"] {
        start(&server, "alice", &org, &ws, &helper, input).await;
    }
    work(&server).await;

    let stats = |workspace: String| {
        let server = &server;
        let org = org.clone();
        async move {
            let (status, body) = server
                .send(call("alice", Method::GET, format!("/v1/workspaces/{workspace}")).naming(&org))
                .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            body["data"]["stats"].clone()
        }
    };
    assert_eq!(
        stats(ws.clone()).await,
        json!({ "agents": 1, "installations": 0, "runs_last_24h": 2 })
    );
    assert_eq!(
        stats(id(&other).to_owned()).await["runs_last_24h"],
        0,
        "a workspace counts its own runs"
    );
    let pool = pool(&server).await;
    sqlx::query("UPDATE runs SET created_at = created_at - 2 * 86400000 WHERE input = 'One'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(stats(ws.clone()).await["runs_last_24h"], 1, "only the last 24 hours");

    let usage = |who: &str, query: &str| {
        let path = format!("/v1/organizations/{org}/usage{query}");
        let who = who.to_owned();
        let server = &server;
        async move { server.send(call(&who, Method::GET, path)).await }
    };
    let (status, report) = usage("val", "?environment=sandbox&days=3").await;
    assert_eq!(status, StatusCode::OK, "{report}");
    let days = report["data"]["days"].as_array().unwrap();
    assert_eq!(days.len(), 3);
    assert_eq!(
        days.iter().map(|day| day["runs"].as_u64().unwrap()).collect::<Vec<_>>(),
        [1, 0, 1],
        "per UTC day, every day listed, today last"
    );
    let today = zupflow_core::time::Timestamp::now().to_string();
    assert_eq!(days[2]["date"], &today[..10]);
    assert_eq!(
        report["data"]["totals"],
        json!({ "runs": 2, "failed_runs": 0, "tool_calls": 0 })
    );
    assert_eq!(report["data"]["environment"], "sandbox");

    let (_, report) = usage("alice", "?environment=sandbox").await;
    assert_eq!(
        report["data"]["days"].as_array().unwrap().len(),
        14,
        "14 days by default"
    );
    let (_, report) = usage("alice", "?environment=sandbox&days=500").await;
    assert_eq!(report["data"]["days"].as_array().unwrap().len(), 90, "90 at most");
    let (_, report) = usage("alice", "?environment=production&days=3").await;
    assert_eq!(report["data"]["totals"]["runs"], 0, "one environment's runs");
    let (status, body) = usage("alice", "").await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, "validation_failed"));
    assert_eq!(body["error"]["fields"]["environment"], "Required.");

    let key = api_key(&server, "alice", &org, "production").await;
    let (status, body) = usage(&key, "?environment=sandbox").await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "environment_mismatch")
    );
    let (status, _) = usage(&key, "?environment=production").await;
    assert_eq!(status, StatusCode::OK);
    let theirs = server.organization("mallory", "larkspur").await;
    let (status, body) = server
        .send(call(
            "mallory",
            Method::GET,
            format!("/v1/organizations/{org}/usage?environment=sandbox"),
        ))
        .await;
    assert_eq!((status, code(&body)), (StatusCode::NOT_FOUND, "not_found"));
    let (_, report) = server
        .send(call(
            "mallory",
            Method::GET,
            format!("/v1/organizations/{theirs}/usage?environment=sandbox"),
        ))
        .await;
    assert_eq!(
        report["data"]["totals"]["runs"], 0,
        "another organization's runs never count"
    );
    pool.close().await;
}

/// Seconds from now until the queued job of `kind` runs.
async fn due_in(pool: &sqlx::SqlitePool, kind: &str) -> (i64, i64) {
    let (attempts, run_at): (i64, i64) =
        sqlx::query_as("SELECT attempts, run_at FROM jobs WHERE kind = ? AND state = 'queued'")
            .bind(kind)
            .fetch_one(pool)
            .await
            .unwrap();
    (attempts, run_at - zupflow_core::time::Timestamp::now().millis())
}

/// A postponed delivery advertises its real queue time without inventing
/// an outbound attempt.
async fn check_retry_time(server: &Server, pool: &sqlx::SqlitePool, org: &str, endpoint: &str, attempts: u64) {
    let organization_id = zupflow_core::ids::OrganizationId::parse(org).unwrap();
    let endpoint_id = zupflow_core::ids::WebhookEndpointId::parse(endpoint).unwrap();
    let next: i64 = sqlx::query_scalar(
        "SELECT next_attempt_at FROM webhook_deliveries \
         WHERE organization_id = ? AND endpoint_id = ? AND state = 'pending'",
    )
    .bind(organization_id.uuid())
    .bind(endpoint_id.uuid())
    .fetch_one(pool)
    .await
    .unwrap();
    let run_at: i64 = sqlx::query_scalar(
        "SELECT run_at FROM jobs WHERE organization_id = ? AND kind = 'deliver_webhook' AND state = 'queued'",
    )
    .bind(organization_id.uuid())
    .fetch_one(pool)
    .await
    .unwrap();
    assert!((run_at - next).abs() < 1_000, "history follows the scheduled job");
    let (status, history) = server
        .send(call(
            "alice",
            Method::GET,
            format!("/v1/organizations/{org}/webhooks/{endpoint}/deliveries?state=pending"),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{history}");
    assert_eq!(history["meta"]["pagination"]["total"], 1);
    let delivery = &history["data"][0];
    assert_eq!(delivery["attempts"], attempts);
    assert_eq!(
        delivery["next_attempt_at"],
        zupflow_core::time::Timestamp::from_millis(next).to_string()
    );
    let (status, records) = server
        .send(call(
            "alice",
            Method::GET,
            format!(
                "/v1/organizations/{org}/webhooks/{endpoint}/deliveries/{}/attempts",
                id(delivery)
            ),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{records}");
    assert_eq!(records["meta"]["pagination"]["total"], attempts);
}

async fn run_now(pool: &sqlx::SqlitePool, kind: &str) {
    sqlx::query("UPDATE jobs SET run_at = 0 WHERE kind = ? AND state = 'queued'")
        .bind(kind)
        .execute(pool)
        .await
        .unwrap();
}

/// The delivery's state, attempts and last error, and its endpoint's last
/// status, failing-since and enabled.
async fn standing(pool: &sqlx::SqlitePool) -> (String, i64, Option<String>, Option<i64>, Option<i64>, i64) {
    sqlx::query_as(
        "SELECT d.state, d.attempts, d.last_error, p.last_status_code, p.failing_since, p.enabled \
         FROM webhook_deliveries d JOIN webhook_endpoints p ON p.id = d.endpoint_id \
         ORDER BY d.created_at DESC, d.rowid DESC LIMIT 1",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// When Zupflow itself can't send (here, its key service can't open the
/// signing secret), nothing counts against the endpoint: no attempt, no last
/// delivery, no failing count. The job backs off and tries again; the
/// delivery's attempt number is its own count; and a job out of claims
/// settles its delivery as failed, still without charging the endpoint.
#[tokio::test]
async fn charges_the_endpoint_only_for_its_own_failures() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let sandbox = server.workspace("alice", &org, "acme-sandbox", "sandbox").await;
    let (endpoint, _) = hook(&server, "alice", &org, "sandbox", HOOK, json!(["run.completed"])).await;
    let ws = id(&sandbox).to_owned();
    let helper = agent(&server, "alice", &org, &ws).await;
    let pool = pool(&server).await;
    let sealed: Vec<u8> = sqlx::query_scalar("SELECT secret_ciphertext FROM webhook_endpoints")
        .fetch_one(&pool)
        .await
        .unwrap();
    let break_secret = || async {
        sqlx::query("UPDATE webhook_endpoints SET secret_ciphertext = randomblob(length(secret_ciphertext))")
            .execute(&pool)
            .await
            .unwrap();
    };
    let mend_secret = || async {
        sqlx::query("UPDATE webhook_endpoints SET secret_ciphertext = ?")
            .bind(&sealed)
            .execute(&pool)
            .await
            .unwrap();
    };
    break_secret().await;

    // The test button answers 503 and records nothing.
    let (status, body) = server
        .send(call(
            "alice",
            Method::POST,
            format!("/v1/organizations/{org}/webhooks/{}/test", id(&endpoint)),
        ))
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        "{body}"
    );
    let (_, list) = server
        .send(call("alice", Method::GET, format!("/v1/organizations/{org}/webhooks")))
        .await;
    assert_eq!(list["data"][0]["last_delivery"], Value::Null);

    // A delivery isn't sent, and isn't an attempt.
    start(&server, "alice", &org, &ws, &helper, "Hello").await;
    work(&server).await;
    assert!(server.providers.requests_to(HOOK).is_empty());
    assert_eq!(
        standing(&pool).await,
        ("pending".to_owned(), 0, None, None, None, 1),
        "no attempt, no last delivery, not failing"
    );
    let (claims, due) = due_in(&pool, "deliver_webhook").await;
    assert_eq!(claims, 1);
    assert!((4_000..=6_000).contains(&due), "backing off from 5 seconds: {due}");
    check_retry_time(&server, &pool, &org, id(&endpoint), 0).await;
    run_now(&pool, "deliver_webhook").await;
    assert!(webhook_delivery::deliver_once(&server.state).await);
    let (claims, due) = due_in(&pool, "deliver_webhook").await;
    assert_eq!(claims, 2);
    assert!((8_500..=11_500).contains(&due), "then 10: {due}");
    check_retry_time(&server, &pool, &org, id(&endpoint), 0).await;

    // Mended, it's the delivery's first attempt, whatever the job's claims.
    mend_secret().await;
    server.providers.script(HOOK, 500, json!({}));
    run_now(&pool, "deliver_webhook").await;
    assert!(webhook_delivery::deliver_once(&server.state).await);
    let sent = server.providers.requests_to(HOOK);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].header("Zupflow-Delivery-Attempt"), Some("1"));
    let (state, attempts, _, last_status, failing_since, _) = standing(&pool).await;
    assert_eq!((state.as_str(), attempts, last_status), ("pending", 1, Some(500)));
    assert!(failing_since.is_some(), "the endpoint's own failure counts");
    break_secret().await;
    run_now(&pool, "deliver_webhook").await;
    assert!(webhook_delivery::deliver_once(&server.state).await);
    check_retry_time(&server, &pool, &org, id(&endpoint), 1).await;
    mend_secret().await;
    server.providers.script(HOOK, 200, json!({}));
    run_now(&pool, "deliver_webhook").await;
    assert!(webhook_delivery::deliver_once(&server.state).await);
    let sent = server.providers.requests_to(HOOK);
    assert_eq!(sent.len(), 2);
    assert_eq!(
        sent[1].header("Zupflow-Delivery-Attempt"),
        Some("2"),
        "the claim that sent nothing wasn't an attempt"
    );
    assert_eq!(
        standing(&pool).await,
        ("succeeded".to_owned(), 2, None, Some(200), None, 1)
    );

    // A job out of claims settles its delivery as failed, and the endpoint
    // keeps its last answer.
    break_secret().await;
    start(&server, "alice", &org, &ws, &helper, "Again").await;
    work(&server).await;
    sqlx::query("UPDATE jobs SET attempts = max_attempts - 1, run_at = 0 WHERE kind = 'deliver_webhook'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(webhook_delivery::deliver_once(&server.state).await);
    assert_eq!(
        standing(&pool).await,
        (
            "failed".to_owned(),
            0,
            Some("secret_unavailable".to_owned()),
            Some(200),
            None,
            1
        )
    );
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'deliver_webhook'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0, "finished, not left to die");
    assert_eq!(server.providers.requests_to(HOOK).len(), 2);
    pool.close().await;
}

/// An endpoint has one delivery in flight: one that finds it busy waits
/// its turn without using an attempt, and is sent once it's free.
#[tokio::test]
async fn waits_for_a_busy_endpoint_without_using_an_attempt() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let sandbox = server.workspace("alice", &org, "acme-sandbox", "sandbox").await;
    let (endpoint, _) = hook(&server, "alice", &org, "sandbox", HOOK, json!(["run.completed"])).await;
    let ws = id(&sandbox).to_owned();
    let helper = agent(&server, "alice", &org, &ws).await;
    let pool = pool(&server).await;
    // Another worker's delivery is in flight.
    sqlx::query("UPDATE webhook_endpoints SET delivery_holder = randomblob(16), delivery_held_until = ?")
        .bind(zupflow_core::time::Timestamp::now().millis() + 60_000)
        .execute(&pool)
        .await
        .unwrap();
    start(&server, "alice", &org, &ws, &helper, "Hello").await;
    work(&server).await;
    assert!(server.providers.requests_to(HOOK).is_empty(), "not while it's busy");
    let (claims, due) = due_in(&pool, "deliver_webhook").await;
    assert_eq!(claims, 0, "waiting isn't an attempt");
    assert!((2_000..=8_000).contains(&due), "about five seconds: {due}");
    assert_eq!(standing(&pool).await.1, 0);
    check_retry_time(&server, &pool, &org, id(&endpoint), 0).await;

    sqlx::query("UPDATE webhook_endpoints SET delivery_holder = NULL, delivery_held_until = NULL")
        .execute(&pool)
        .await
        .unwrap();
    run_now(&pool, "deliver_webhook").await;
    assert!(webhook_delivery::deliver_once(&server.state).await);
    let sent = server.providers.requests_to(HOOK);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].header("Zupflow-Delivery-Attempt"), Some("1"));
    let held: Option<i64> = sqlx::query_scalar("SELECT delivery_held_until FROM webhook_endpoints")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(held, None, "let go once sent");
    pool.close().await;
}

/// An announcement outlasts a store that's unavailable, backing off up to an
/// hour; given up on at last, its job is buried, not dropped.
#[tokio::test]
async fn backs_off_an_announcement_and_buries_it_at_last() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let sandbox = server.workspace("alice", &org, "acme-sandbox", "sandbox").await;
    hook(&server, "alice", &org, "sandbox", HOOK, json!(["run.completed"])).await;
    let ws = id(&sandbox).to_owned();
    let helper = agent(&server, "alice", &org, &ws).await;
    let pool = pool(&server).await;
    sqlx::query(
        "CREATE TRIGGER refuse_events BEFORE INSERT ON webhook_events BEGIN SELECT RAISE(ABORT, 'refused'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    start(&server, "alice", &org, &ws, &helper, "Hello").await;
    work(&server).await;
    let (claims, due) = due_in(&pool, "announce_webhook_event").await;
    assert_eq!(claims, 1);
    assert!((4_000..=6_000).contains(&due), "{due}");
    sqlx::query("UPDATE jobs SET attempts = 19, run_at = 0 WHERE kind = 'announce_webhook_event'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(webhook_delivery::announce_once(&server.state).await);
    let (claims, due) = due_in(&pool, "announce_webhook_event").await;
    assert_eq!(claims, 20);
    assert!((3_200_000..=4_000_000).contains(&due), "an hour at most: {due}");

    sqlx::query("UPDATE jobs SET attempts = max_attempts - 1, run_at = 0 WHERE kind = 'announce_webhook_event'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(webhook_delivery::announce_once(&server.state).await);
    let (state, reason): (String, Option<String>) =
        sqlx::query_as("SELECT state, reason FROM jobs WHERE kind = 'announce_webhook_event'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((state.as_str(), reason.as_deref()), ("dead", Some("store_unavailable")));
    assert!(server.providers.requests_to(HOOK).is_empty());
    pool.close().await;
}
