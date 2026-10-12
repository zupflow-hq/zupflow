//! End to end, against a fake MCP app with its own OAuth: the development
//! contract server (the real router, stores, workers and safe outbound path,
//! with the scripted model) and Taskboard (`zupflow-taskboard`), each on a
//! loopback port, reached over HTTP and HTTPS as a browser and a customer's
//! backend would.
//!
//! An organization adds Taskboard as an `mcp_oauth` app in Sandbox; a person
//! connects it through hosted Connect (sign-in discovered from the MCP
//! server, a client registered, consent with PKCE), denying once, then
//! allowing; its tools are discovered; an agent is granted them within the
//! organization's levels; runs call them on Taskboard with the
//! installation's token, pausing for approval where the policy says; and an
//! expired token refreshes once across concurrent workers; a revoked refresh
//! token requires reconnect with an audit event and webhook; reconnecting
//! clears it, forgotten clients register again, and callback issuers are checked.
//! A second journey verifies signed webhook history and resends against
//! Taskboard's receiver across organizations, workspaces and environments.
//! A third rolls a signing secret over HTTP and verifies both signatures
//! during overlap, without changing event identity or neighboring scopes.
//!
//! The browser is a headless HTTP client following each redirect by hand and
//! answering the consent page's form, so every hop can be checked.
//!
//! Needs the `local-apps` feature (`--features zupflow-contract/local-apps`),
//! as `pnpm check` passes it.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use zupflow_core::time::Timestamp;
use zupflow_core::webhooks::signature;
use zupflow_taskboard::{Options, Running, WebhookRequest};

const OWNER: &str = "contract:owner";

/// The backend and Taskboard, running; stopped when dropped.
struct Stack {
    taskboard: Running,
    backend: Child,
    base: String,
    directory: PathBuf,
    /// Taskboard's certificate is the only one trusted, and its name is
    /// pinned to loopback, as the backend's own client is configured.
    http: Client,
}

impl Drop for Stack {
    fn drop(&mut self) {
        let _ = self.backend.kill();
        let _ = self.backend.wait();
        // A failing test shows what the backend said.
        if std::thread::panicking() {
            print_backend_log(&self.directory);
        }
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// The backend's standard error, from its file.
fn print_backend_log(directory: &std::path::Path) {
    let log = std::fs::read_to_string(directory.join("serve.err")).unwrap_or_default();
    eprintln!("--- the contract server's standard error ---\n{log}--- end ---");
}

impl Stack {
    async fn start() -> Self {
        let taskboard = zupflow_taskboard::start(Options::default()).await.unwrap();
        let directory = std::env::temp_dir().join(format!(
            "zupflow-taskboard-e2e-{}-{}",
            std::process::id(),
            taskboard.port
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let certificate = directory.join("taskboard.pem");
        std::fs::write(&certificate, &taskboard.certificate_pem).unwrap();
        let database = directory.join("contract.sqlite");
        let bin = env!("CARGO_BIN_EXE_zupflow-contract-server");
        let reset = Command::new(bin).arg("reset").arg(&database).status().unwrap();
        assert!(reset.success(), "the fixture was restored");
        let stderr = std::fs::File::create(directory.join("serve.err")).unwrap();
        let mut backend = Command::new(bin)
            .arg("serve")
            .arg(&database)
            .arg("--local-app")
            .arg(format!(
                "taskboard.example:{}={}",
                taskboard.port,
                certificate.display()
            ))
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .unwrap();
        // It announces the port it got, within 30 seconds.
        let stdout = backend.stdout.take().unwrap();
        let announced = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::task::spawn_blocking(move || {
                let mut line = String::new();
                BufReader::new(stdout).read_line(&mut line).ok()?;
                line.trim().strip_prefix("listening ")?.parse::<u16>().ok()
            }),
        )
        .await;
        let Ok(Ok(Some(port))) = announced else {
            let _ = backend.kill();
            let _ = backend.wait();
            print_backend_log(&directory);
            panic!("the contract server didn't announce its port within 30 seconds: {announced:?}");
        };
        let http = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .resolve("taskboard.example", SocketAddr::from(([127, 0, 0, 1], taskboard.port)))
            .tls_certs_only([reqwest::Certificate::from_pem(taskboard.certificate_pem.as_bytes()).unwrap()])
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap();
        Self {
            taskboard,
            backend,
            base: format!("http://localhost:{port}"),
            directory,
            http,
        }
    }

    /// An API request as `who` (a persona's token or an API key), naming
    /// the organization when given.
    async fn api(
        &self,
        who: &str,
        method: Method,
        path: &str,
        org: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = self.http.request(method, format!("{}{path}", self.base));
        // Hosted Connect is called with no credential: the link is the reference.
        if !who.is_empty() {
            request = request.bearer_auth(who);
        }
        if let Some(org) = org {
            request = request.header("Zupflow-Organization", org);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// What a browser gets from `url`: status, `Location` and body.
    async fn browse(
        &self,
        method: Method,
        url: &str,
        form: Option<&[(&str, &str)]>,
    ) -> (StatusCode, Option<String>, String) {
        let mut request = self.http.request(method, url);
        if let Some(form) = form {
            request = request.form(form);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get("location")
            .map(|value| value.to_str().unwrap().to_owned());
        (status, location, response.text().await.unwrap())
    }
}

fn data(body: &Value) -> &Value {
    &body["data"]
}

fn id(value: &Value) -> String {
    value["id"].as_str().unwrap().to_owned()
}

fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

fn query(url: &str) -> BTreeMap<String, String> {
    url::Url::parse(url).unwrap().query_pairs().into_owned().collect()
}

fn hidden(page: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let start = page.find(&marker).unwrap() + marker.len();
    let end = start + page[start..].find('"').unwrap();
    page[start..end].to_owned()
}

/// Polls `check` until it answers something, for up to 30 seconds: the
/// workers look for work every second.
async fn eventually<T, F, Fut>(what: &str, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(value) = check().await {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The organization, the Sandbox workspace and the Taskboard app, as the
/// owner sets them up in the Console.
struct Tenant {
    org: String,
    ws: String,
    app: String,
}

async fn tenant(stack: &Stack) -> Tenant {
    let (status, viewer) = stack.api(OWNER, Method::GET, "/v1/me", None, None).await;
    assert_eq!(status, StatusCode::OK, "{viewer}");
    let org = data(&viewer)["organizations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|organization| organization["slug"] == "northwind")
        .map(id)
        .unwrap();
    let (status, workspace) = stack
        .api(
            OWNER,
            Method::POST,
            &format!("/v1/organizations/{org}/workspaces"),
            None,
            Some(json!({ "name": "Taskboard trial", "slug": "taskboard-trial", "environment": "sandbox", "external_id": null })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{workspace}");
    let ws = id(data(&workspace));

    let (status, created) = stack
        .api(
            OWNER,
            Method::POST,
            &format!("/v1/organizations/{org}/apps"),
            None,
            Some(json!({ "kind": "mcp", "name": "Taskboard", "key": "taskboard_live", "description": "Tasks, for testing.", "auth_type": "mcp_oauth" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let app = id(data(&created));
    let (status, body) = stack
        .api(
            OWNER,
            Method::PATCH,
            &format!("/v1/organizations/{org}/apps/{app}"),
            None,
            Some(json!({ "auth": {
                "type": "mcp_oauth",
                "scopes": [
                    { "name": "tasks:read", "description": "See tasks" },
                    { "name": "tasks:write", "description": "Change tasks" },
                ],
                "identity_url": stack.taskboard.taskboard.identity_url(),
            } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = stack
        .api(
            OWNER,
            Method::PUT,
            &format!("/v1/organizations/{org}/apps/{app}/environments/sandbox/configuration"),
            None,
            Some(json!({ "mcp_server_url": stack.taskboard.taskboard.mcp_url() })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(data(&body)["status"], "ready", "{body}");
    Tenant { org, ws, app }
}

impl Tenant {
    /// Starts a hosted Connect session: its reference.
    async fn session(&self, stack: &Stack, installation: Option<&str>) -> String {
        let (status, created) = stack
            .api(
                OWNER,
                Method::POST,
                &format!("/v1/workspaces/{}/connect-sessions", self.ws),
                Some(&self.org),
                Some(json!({
                    "app_id": self.app,
                    "intent": if installation.is_some() { "reconnect" } else { "create" },
                    "label": "Robin's board",
                    "installation_id": installation,
                    "return_url": null,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        let url = data(&created)["url"].as_str().unwrap().to_owned();
        assert!(url.starts_with(&format!("{}/connect/", stack.base)), "{url}");
        url.rsplit('/').next().unwrap().to_owned()
    }

    /// Opens the hosted page, starts consent as its script does, and lands
    /// on Taskboard's consent page: the page.
    async fn consent_page(&self, stack: &Stack, reference: &str) -> String {
        let (status, _, page) = stack
            .browse(Method::GET, &format!("{}/connect/{reference}", stack.base), None)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(page.contains("Taskboard"), "the hosted page names the app");
        let (status, started) = stack
            .api(
                "",
                Method::POST,
                &format!("/v1/connect/sessions/{reference}/authorize"),
                None,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{started}");
        let authorization_url = data(&started)["authorization_url"].as_str().unwrap().to_owned();
        let asked = query(&authorization_url);
        let board = &stack.taskboard.taskboard;
        assert!(authorization_url.starts_with(&format!("{}/authorize?", board.base_url())));
        assert_eq!(asked["code_challenge_method"], "S256");
        assert_eq!(asked["resource"], board.mcp_url());
        assert_eq!(asked["scope"], "tasks:read tasks:write");
        assert_eq!(asked["redirect_uri"], format!("{}/connect/oauth/callback", stack.base));
        let (status, _, page) = stack.browse(Method::GET, &authorization_url, None).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert!(page.contains("Zupflow wants to use your Taskboard"), "{page}");
        page
    }

    /// Answers the consent page and follows the browser back through the
    /// callback to the hosted page; the session as the hosted API reads it.
    async fn answer(&self, stack: &Stack, reference: &str, page: &str, decision: &str) -> Value {
        let callback = self.decide_consent(stack, page, decision).await;
        self.follow(stack, reference, &callback).await
    }

    /// Answers the consent page: the callback Taskboard sends the browser to.
    async fn decide_consent(&self, stack: &Stack, page: &str, decision: &str) -> String {
        let request = hidden(page, "request");
        let (status, location, _) = stack
            .browse(
                Method::POST,
                &format!("{}/authorize", stack.taskboard.taskboard.base_url()),
                Some(&[("request", &request), ("decision", decision)]),
            )
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let callback = location.unwrap();
        assert!(
            callback.starts_with(&format!("{}/connect/oauth/callback?", stack.base)),
            "{callback}"
        );
        callback
    }

    /// Follows the callback back to the hosted page; the session as the
    /// hosted API reads it.
    async fn follow(&self, stack: &Stack, reference: &str, callback: &str) -> Value {
        let (status, location, _) = stack.browse(Method::GET, callback, None).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location.unwrap(), format!("{}/connect/{reference}", stack.base));
        let (status, session) = stack
            .api(
                "",
                Method::GET,
                &format!("/v1/connect/sessions/{reference}"),
                None,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{session}");
        data(&session).clone()
    }

    async fn installation(&self, stack: &Stack, installation: &str) -> Value {
        let (status, body) = stack
            .api(
                OWNER,
                Method::GET,
                &format!("/v1/workspaces/{}/installations/{installation}", self.ws),
                Some(&self.org),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        data(&body).clone()
    }

    async fn start_run(&self, stack: &Stack, agent: &str, input: &str) -> String {
        let (status, run) = stack
            .api(
                OWNER,
                Method::POST,
                &format!("/v1/workspaces/{}/agents/{agent}/runs", self.ws),
                Some(&self.org),
                Some(json!({ "input": input })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{run}");
        id(data(&run))
    }

    /// Waits for the run to reach `status`; the run.
    async fn run_reaches(&self, stack: &Stack, run: &str, status: &str) -> Value {
        eventually(&format!("run {run} to be {status}"), || async {
            let (_, body) = stack
                .api(
                    OWNER,
                    Method::GET,
                    &format!("/v1/workspaces/{}/runs/{run}", self.ws),
                    Some(&self.org),
                    None,
                )
                .await;
            let found = data(&body)["status"].as_str().unwrap_or_default();
            assert!(
                found == status || ["queued", "running", "waiting_for_approval"].contains(&found),
                "run {run} is {found}, waiting for {status}: {body}"
            );
            (found == status).then(|| data(&body).clone())
        })
        .await
    }

    async fn events(&self, stack: &Stack, run: &str) -> Vec<Value> {
        let (status, body) = stack
            .api(
                OWNER,
                Method::GET,
                &format!("/v1/workspaces/{}/runs/{run}/events", self.ws),
                Some(&self.org),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        data(&body).as_array().unwrap().clone()
    }

    async fn decide(&self, stack: &Stack, who: &str, run: &str, call_id: &str, decision: &str) -> (StatusCode, Value) {
        stack
            .api(
                who,
                Method::POST,
                &format!("/v1/workspaces/{}/runs/{run}/approvals", self.ws),
                Some(&self.org),
                Some(json!({ "call_id": call_id, "decision": decision })),
            )
            .await
    }

    async fn levels(&self, stack: &Stack, levels: Value) {
        let (status, body) = stack
            .api(
                OWNER,
                Method::PATCH,
                &format!("/v1/organizations/{}/apps/{}/tool-levels", self.org, self.app),
                None,
                Some(json!({ "levels": levels })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
}

fn types(events: &[Value]) -> Vec<&str> {
    events.iter().map(|event| event["type"].as_str().unwrap()).collect()
}

/// The pending approval's call, from the run's last event.
fn awaiting(events: &[Value], tool: &str) -> String {
    let requested = events.last().unwrap();
    assert_eq!(requested["type"], "approval.requested");
    assert_eq!(requested["data"]["tool"], tool);
    requested["data"]["call_id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn connects_grants_and_runs_against_a_fake_mcp_app() {
    let stack = Stack::start().await;
    let board = stack.taskboard.taskboard.clone();
    let t = tenant(&stack).await;
    let callback = format!("{}/connect/oauth/callback", stack.base);

    // --- Consent denied: the session fails, nothing is installed ----------
    let reference = t.session(&stack, None).await;
    let page = t.consent_page(&stack, &reference).await;
    // Sign-in was discovered from the MCP server's challenge, and one public
    // client registered for the deployment's callback.
    let clients = board.registered_clients();
    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0].client_name.as_deref(), Some("Zupflow"));
    assert_eq!(clients[0].redirect_uris, std::slice::from_ref(&callback));
    let client_id = clients[0].client_id.clone();
    let denied = t.answer(&stack, &reference, &page, "deny").await;
    assert_eq!(
        (&denied["status"], &denied["failure_code"]),
        (&json!("failed"), &json!("consent_denied")),
        "{denied}"
    );
    assert!(board.token_requests().is_empty(), "nothing exchanged");
    let (_, installations) = stack
        .api(
            OWNER,
            Method::GET,
            &format!("/v1/workspaces/{}/installations", t.ws),
            Some(&t.org),
            None,
        )
        .await;
    assert_eq!(data(&installations), &json!([]));

    // A foreign issuer, including on an error, and a missing advertised iss
    // fail before the token endpoint or registration invalidation.
    for issuer in [
        zupflow_taskboard::ResponseIssuer::Foreign,
        zupflow_taskboard::ResponseIssuer::Missing,
    ] {
        let reference = t.session(&stack, None).await;
        let page = t.consent_page(&stack, &reference).await;
        board.set_response_issuer(issuer);
        let failed = t.answer(&stack, &reference, &page, "approve").await;
        board.set_response_issuer(zupflow_taskboard::ResponseIssuer::Correct);
        assert_eq!(failed["failure_code"], "authorization_failed");
        assert!(board.token_requests().is_empty());
    }
    let reference = t.session(&stack, None).await;
    let page = t.consent_page(&stack, &reference).await;
    board.set_response_issuer(zupflow_taskboard::ResponseIssuer::Foreign);
    let callback_url = t.decide_consent(&stack, &page, "deny").await;
    board.set_response_issuer(zupflow_taskboard::ResponseIssuer::Correct);
    let failed = t
        .follow(
            &stack,
            &reference,
            &callback_url.replace("access_denied", "invalid_client"),
        )
        .await;
    assert_eq!(failed["failure_code"], "authorization_failed");
    assert!(board.token_requests().is_empty());

    // --- Allowed: installed, as the account Taskboard names ---------------
    let reference = t.session(&stack, None).await;
    let page = t.consent_page(&stack, &reference).await;
    assert_eq!(
        board.registered_clients().len(),
        1,
        "the client is registered once and reused"
    );
    let callback_url = t.decide_consent(&stack, &page, "approve").await;
    // A tampered state matches no sign-in: refused, nothing exchanged, and
    // the real callback still completes.
    let mut tampered = url::Url::parse(&callback_url).unwrap();
    let pairs: Vec<(String, String)> = tampered.query_pairs().into_owned().collect();
    tampered
        .query_pairs_mut()
        .clear()
        .extend_pairs(pairs.iter().map(|(name, value)| {
            let value = if name == "state" {
                let last = if value.ends_with('A') { 'B' } else { 'A' };
                format!("{}{last}", &value[..value.len() - 1])
            } else {
                value.clone()
            };
            (name.clone(), value)
        }));
    assert_ne!(tampered.as_str(), callback_url);
    let (status, _, body) = stack.browse(Method::GET, tampered.as_str(), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.starts_with("<!doctype html>"), "{body}");
    assert!(body.contains("invalid_oauth_state"));
    assert!(
        board.token_requests().is_empty(),
        "nothing exchanged for a tampered state"
    );
    let completed = t.follow(&stack, &reference, &callback_url).await;
    assert_eq!(completed["status"], "completed", "{completed}");
    let requests = board.token_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        (
            requests[0].grant_type.as_str(),
            requests[0].client_id.as_deref(),
            requests[0].resource.as_deref(),
            requests[0].error.as_deref()
        ),
        (
            "authorization_code",
            Some(client_id.as_str()),
            Some(board.mcp_url().as_str()),
            None
        ),
        "exchanged with the verifier, as the public client, for the MCP server"
    );
    // The same callback again: its state was spent, so the code is never
    // sent a second time.
    let (status, _, body) = stack.browse(Method::GET, &callback_url, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.starts_with("<!doctype html>"), "{body}");
    assert!(body.contains("invalid_oauth_state"));
    assert_eq!(board.token_requests().len(), 1, "a replayed callback exchanges nothing");
    let installation_id = completed["installation_id"].as_str().unwrap().to_owned();
    let installation = t.installation(&stack, &installation_id).await;
    assert_eq!(installation["status"], "connected", "{installation}");
    assert_eq!(
        installation["connection"]["account"],
        json!({ "external_id": "tb-user-1", "display_name": "Robin Park", "email": "robin@taskboard.example" })
    );
    assert_eq!(
        installation["connection"]["granted_scopes"],
        json!(["tasks:read", "tasks:write"])
    );
    assert_eq!(installation["connection"]["credential_version"], 1);

    // --- Its tools are discovered, with levels suggested from their hints -
    let tools = eventually("the installation's tools", || async {
        let (_, body) = stack
            .api(
                OWNER,
                Method::GET,
                &format!("/v1/workspaces/{}/installations/{installation_id}/tools", t.ws),
                Some(&t.org),
                None,
            )
            .await;
        let tools = data(&body).as_array().cloned().unwrap_or_default();
        (tools.len() == 3).then_some(tools)
    })
    .await;
    let mut names: Vec<&str> = tools.iter().map(|tool| tool["name"].as_str().unwrap()).collect();
    names.sort_unstable();
    assert_eq!(names, ["create_task", "delete_task", "list_tasks"]);
    // The listing and suggested levels commit together.
    let levels = eventually("the suggested levels", || async {
        let (status, body) = stack
            .api(
                OWNER,
                Method::GET,
                &format!("/v1/organizations/{}/apps/{}/tool-levels", t.org, t.app),
                None,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        (data(&body).as_array().map_or(0, Vec::len) == 3).then_some(body)
    })
    .await;
    let suggested: BTreeMap<String, (String, bool, bool)> = data(&levels)
        .as_array()
        .unwrap()
        .iter()
        .map(|level| {
            (
                level["tool"].as_str().unwrap().to_owned(),
                (
                    level["level"].as_str().unwrap().to_owned(),
                    level["confirmed"].as_bool().unwrap(),
                    level["destructive"].as_bool().unwrap(),
                ),
            )
        })
        .collect();
    assert_eq!(
        suggested,
        BTreeMap::from([
            ("create_task".to_owned(), ("approve".to_owned(), false, false)),
            ("delete_task".to_owned(), ("approve".to_owned(), false, true)),
            ("list_tasks".to_owned(), ("allow".to_owned(), false, false)),
        ]),
        "suggested from the hints, unconfirmed"
    );

    // --- An agent, granted tools within the organization's levels ---------
    t.levels(
        &stack,
        json!([
            { "tool": "list_tasks", "level": "allow" },
            { "tool": "create_task", "level": "approve" },
            { "tool": "delete_task", "level": "deny" },
        ]),
    )
    .await;
    let (status, agent) = stack
        .api(
            OWNER,
            Method::POST,
            &format!("/v1/workspaces/{}/agents", t.ws),
            Some(&t.org),
            Some(json!({ "name": "Planner", "description": "", "model": "model-balanced", "instructions": "Keep the board tidy." })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{agent}");
    let agent = id(data(&agent));
    let grants = format!("/v1/workspaces/{}/agents/{agent}/grants", t.ws);
    for (tools, approval_required, refusal) in [
        (
            json!(["list_tasks", "delete_task"]),
            json!(["delete_task"]),
            "tool_denied",
        ),
        (
            json!(["list_tasks", "create_task"]),
            json!([]),
            "approval_required_by_policy",
        ),
    ] {
        let (status, body) = stack
            .api(
                OWNER,
                Method::POST,
                &grants,
                Some(&t.org),
                Some(json!({ "installation_id": installation_id, "tools": tools, "approval_required": approval_required })),
            )
            .await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::UNPROCESSABLE_ENTITY, refusal),
            "{body}"
        );
    }
    let (status, grant) = stack
        .api(
            OWNER,
            Method::POST,
            &grants,
            Some(&t.org),
            Some(json!({ "installation_id": installation_id, "tools": ["list_tasks", "create_task"], "approval_required": ["create_task"] })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{grant}");
    let grant = id(data(&grant));

    // The customer's backend decides approvals, with a Sandbox key.
    let (status, key) = stack
        .api(
            OWNER,
            Method::POST,
            &format!("/v1/organizations/{}/api-keys", t.org),
            None,
            Some(json!({ "name": "Backend", "environment": "sandbox" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{key}");
    let key = data(&key)["secret"].as_str().unwrap().to_owned();

    // --- A run: list_tasks runs, create_task waits for approval -----------
    let run = t
        .start_run(
            &stack,
            &agent,
            "call list_tasks {}\ncall create_task {\"title\":\"Send the quarterly report\"}",
        )
        .await;
    t.run_reaches(&stack, &run, "waiting_for_approval").await;
    let token = board.access_tokens_issued(&client_id).last().unwrap().clone();
    let calls = board.tool_calls();
    assert_eq!(calls.len(), 1, "nothing that needs approval is sent before it");
    assert_eq!(
        (
            calls[0].tool.as_str(),
            calls[0].client_id.as_str(),
            calls[0].access_token.as_str()
        ),
        ("list_tasks", client_id.as_str(), token.as_str()),
        "sent to Taskboard with the installation's token"
    );
    let events = t.events(&stack, &run).await;
    let call_id = awaiting(&events, "create_task");
    assert_eq!(
        events.last().unwrap()["data"]["arguments"],
        json!({ "title": "Send the quarterly report" })
    );
    // The Console observes approvals; it never resolves them.
    let (status, body) = t.decide(&stack, OWNER, &run, &call_id, "approved").await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "approval_requires_api_key")
    );
    let (status, body) = t.decide(&stack, &key, &run, &call_id, "approved").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let done = t.run_reaches(&stack, &run, "completed").await;
    assert_eq!(done["usage"]["tool_calls"], 2, "{done}");
    let output = done["output"].as_str().unwrap();
    assert!(
        output.contains("list_tasks: ") && output.contains("create_task: "),
        "{output}"
    );
    let calls = board.tool_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        (
            calls[1].tool.as_str(),
            &calls[1].arguments,
            calls[1].access_token.as_str()
        ),
        (
            "create_task",
            &json!({ "title": "Send the quarterly report" }),
            token.as_str()
        )
    );
    let tasks = board.tasks(&client_id);
    assert_eq!(tasks.len(), 1);
    assert_eq!(
        (tasks[0].id.as_str(), tasks[0].title.as_str()),
        ("TB-1", "Send the quarterly report")
    );
    let events = t.events(&stack, &run).await;
    assert_eq!(
        types(&events),
        [
            "run.queued",
            "run.started",
            "message.assistant",
            "tool_call.started",
            "tool_call.completed",
            "approval.requested",
            "approval.resolved",
            "tool_call.started",
            "tool_call.completed",
            "message.assistant",
            "run.completed",
        ]
    );
    let completed_call = events
        .iter()
        .find(|event| event["type"] == "tool_call.completed")
        .unwrap();
    assert_eq!(completed_call["data"]["installation_id"], installation_id.as_str());
    assert_eq!(completed_call["data"]["app_name"], "Taskboard");

    // --- A destructive tool: approved by policy only, and denied here -----
    t.levels(&stack, json!([{ "tool": "delete_task", "level": "approve" }]))
        .await;
    let (status, body) = stack
        .api(
            OWNER,
            Method::PATCH,
            &format!("{grants}/{grant}"),
            Some(&t.org),
            Some(json!({ "tools": ["list_tasks", "create_task", "delete_task"], "approval_required": ["create_task", "delete_task"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let run = t.start_run(&stack, &agent, "call delete_task {\"id\":\"TB-1\"}").await;
    t.run_reaches(&stack, &run, "waiting_for_approval").await;
    let call_id = awaiting(&t.events(&stack, &run).await, "delete_task");
    let (status, body) = t.decide(&stack, &key, &run, &call_id, "denied").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let done = t.run_reaches(&stack, &run, "completed").await;
    assert!(
        done["output"]
            .as_str()
            .unwrap()
            .starts_with("delete_task failed: A person denied this call"),
        "{done}"
    );
    assert_eq!(board.tool_calls().len(), 2, "a denied call is never sent");
    assert_eq!(board.tasks(&client_id).len(), 1);

    // Signed webhook delivery uses the real outbound path too.
    let (status, body) = stack
        .api(
            OWNER,
            Method::POST,
            &format!("/v1/organizations/{}/webhooks", t.org),
            None,
            Some(json!({
                "url": format!("{}/webhooks", board.base_url()), "environment":"sandbox",
                "events":["installation.reconnect_required"], "description":"Reconnect notifications"
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Expire only at the server: the first 401 is refreshed once. Hold the
    // exchange open while the independent discovery worker hits the same
    // expired token. Both must use one rotation, or Taskboard revokes the family.
    board.expire_access_tokens();
    board.set_refresh_delay(Duration::from_secs(3));
    let unauthorized = board.unauthorized_requests();
    let run = t.start_run(&stack, &agent, "call list_tasks {}").await;
    eventually("the run's refresh in flight", || async {
        (board.refreshes_in_flight() == 1).then_some(())
    })
    .await;
    let (status, body) = stack
        .api(
            OWNER,
            Method::POST,
            &format!("/v1/workspaces/{}/installations/{installation_id}/refresh-tools", t.ws),
            Some(&t.org),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    eventually("discovery overlapping the run's refresh", || async {
        (board.unauthorized_requests() >= unauthorized + 2 && board.refreshes_in_flight() == 1).then_some(())
    })
    .await;
    t.run_reaches(&stack, &run, "completed").await;
    board.set_refresh_delay(Duration::ZERO);
    let refreshes = || {
        board
            .token_requests()
            .iter()
            .filter(|r| r.grant_type == "refresh_token")
            .count()
    };
    assert_eq!(refreshes(), 1, "concurrent workers exchange a rotating token once");
    assert_eq!(
        t.installation(&stack, &installation_id).await["connection"]["credential_version"],
        2
    );
    assert_eq!(board.tool_calls().len(), 3, "the refused call did not execute twice");

    // A revoked refresh token durably flags the installation, once, with
    // both its webhook and its audit event in the same transition.
    board.revoke_all_tokens();
    let run = t.start_run(&stack, &agent, "call list_tasks {}").await;
    let failed = t.run_reaches(&stack, &run, "failed").await;
    assert_eq!(failed["error"]["code"], "installation_reconnect_required", "{failed}");
    assert_eq!(
        t.installation(&stack, &installation_id).await["status"],
        "reconnect_required"
    );
    eventually("the reconnect webhook", || async {
        board
            .webhooks()
            .into_iter()
            .find(|event| event["type"] == "installation.reconnect_required")
    })
    .await;
    let event = board.webhooks().pop().unwrap();
    assert_eq!(
        event["data"],
        json!({"object":"installation", "id":installation_id, "app_id":t.app, "status":"reconnect_required"})
    );
    assert_eq!(event["organization_id"], t.org);
    assert_eq!(event["workspace"]["id"], t.ws);
    assert_eq!(event["environment"], "sandbox");
    let (status, body) = stack
        .api(
            OWNER,
            Method::GET,
            &format!("/v1/organizations/{}/audit-events?workspace_id={}", t.org, t.ws),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        data(&body)
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["action"] == "installation.reconnect_required" && e["target"]["id"] == installation_id)
            .count(),
        1
    );
    let requests = board.token_requests().len();
    let run = t.start_run(&stack, &agent, "call list_tasks {}").await;
    t.run_reaches(&stack, &run, "failed").await;
    assert_eq!(
        board.token_requests().len(),
        requests,
        "a failed refresh is never reused"
    );
    assert_eq!(board.webhooks().len(), 1);

    // Reconnect clears the flag and preserves the original account and grants.
    let reference = t.session(&stack, Some(&installation_id)).await;
    let page = t.consent_page(&stack, &reference).await;
    let reconnected = t.answer(&stack, &reference, &page, "approve").await;
    assert_eq!(reconnected["status"], "completed", "{reconnected}");
    let installation = t.installation(&stack, &installation_id).await;
    assert_eq!(installation["status"], "connected");
    assert_eq!(installation["connection"]["credential_version"], 3);
    let run = t.start_run(&stack, &agent, "call list_tasks {}").await;
    let done = t.run_reaches(&stack, &run, "completed").await;
    assert!(
        done["output"]
            .as_str()
            .unwrap()
            .contains("TB-1 [open] Send the quarterly report")
    );

    // A 403 is never treated as a 401: no refresh or automatic replay.
    board.set_mcp_status(Some(403));
    let requests = board.token_requests().len();
    let run = t.start_run(&stack, &agent, "call list_tasks {}").await;
    t.run_reaches(&stack, &run, "completed").await;
    assert_eq!(board.token_requests().len(), requests);
    assert_eq!(t.installation(&stack, &installation_id).await["status"], "connected");
    board.set_mcp_status(None);

    // A server that forgot its dynamic client reports invalid_client on a
    // refresh; the next Connect registers again in the same app scope.
    board.forget_clients();
    board.expire_access_tokens();
    let run = t.start_run(&stack, &agent, "call list_tasks {}").await;
    t.run_reaches(&stack, &run, "failed").await;
    assert_eq!(
        board.token_requests().last().unwrap().error.as_deref(),
        Some("invalid_client")
    );
    let reference = t.session(&stack, Some(&installation_id)).await;
    let page = t.consent_page(&stack, &reference).await;
    let new_client = board.registered_clients()[0].client_id.clone();
    assert_ne!(new_client, client_id);
    let reconnected = t.answer(&stack, &reference, &page, "approve").await;
    assert_eq!(reconnected["status"], "completed");
    assert_eq!(t.installation(&stack, &installation_id).await["status"], "connected");

    // Discovery receives a persistent 401 after one successful refresh: it
    // performs the same reconnect transition even when no run is involved.
    board.set_mcp_status(Some(401));
    let before = refreshes();
    let reference = t.session(&stack, Some(&installation_id)).await;
    let page = t.consent_page(&stack, &reference).await;
    let reconnected = t.answer(&stack, &reference, &page, "approve").await;
    assert_eq!(reconnected["status"], "completed");
    eventually("discovery's reconnect flag", || async {
        let i = t.installation(&stack, &installation_id).await;
        (i["status"] == "reconnect_required").then_some(())
    })
    .await;
    assert_eq!(refreshes(), before + 1, "one refresh even on a persistent 401");
    board.set_mcp_status(None);

    // Every token request named the MCP server as its resource (RFC 8707).
    assert!(
        board
            .token_requests()
            .iter()
            .all(|request| request.resource.as_deref() == Some(board.mcp_url().as_str())),
        "{:?}",
        board.token_requests()
    );
}

/// An endpoint of this journey, separate from the frozen reference history.
async fn history_hook(stack: &Stack, who: &str, org: &str, environment: &str) -> (String, String) {
    subscribed_hook(stack, who, org, environment, &["run.completed"]).await
}

async fn subscribed_hook(stack: &Stack, who: &str, org: &str, environment: &str, events: &[&str]) -> (String, String) {
    let (status, created) = stack
        .api(
            who,
            Method::POST,
            &format!("/v1/organizations/{org}/webhooks"),
            None,
            Some(json!({
                "url": format!("{}/webhooks", stack.taskboard.taskboard.base_url()),
                "environment": environment, "events": events, "description": "History journey",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    (
        id(&data(&created)["endpoint"]),
        data(&created)["signing_secret"].as_str().unwrap().to_owned(),
    )
}

fn verify_webhook(request: &WebhookRequest, secrets: &[&str]) -> Value {
    let header = request.signature.as_deref().unwrap();
    let mut parts = header.split(',');
    let time: i64 = parts.next().unwrap().strip_prefix("t=").unwrap().parse().unwrap();
    let mut actual: Vec<_> = parts
        .map(|part| {
            assert!(part.starts_with("v1="), "only one timestamp, followed by v1 signatures");
            part.to_owned()
        })
        .collect();
    let mut expected: Vec<_> = secrets
        .iter()
        .map(|secret| {
            signature(secret.as_bytes(), time, &request.body)
                .split_once(',')
                .unwrap()
                .1
                .to_owned()
        })
        .collect();
    actual.sort_unstable();
    expected.sort_unstable();
    assert_eq!(actual, expected, "every secret signs the same timestamp and raw body");
    assert!((time - Timestamp::now().millis() / 1000).abs() < 60);
    let event: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(request.event_id.as_deref(), event["id"].as_str());
    assert_eq!(request.event_type.as_deref(), event["type"].as_str());
    assert_eq!(request.attempt.as_deref(), Some("1"));
    event
}

/// Checks the readable history against the actual signed HTTP request.
async fn check_history(
    stack: &Stack,
    who: &str,
    org: &str,
    hook: &str,
    summary: &Value,
    request: &WebhookRequest,
    secrets: &[&str],
) {
    let event = verify_webhook(request, secrets);
    let path = format!("/v1/organizations/{org}/webhooks/{hook}/deliveries/{}", id(summary));
    let (status, detail) = stack.api(who, Method::GET, &path, None, None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(data(&detail)["event"], event, "history is the event actually sent");
    assert_eq!(data(&detail)["attempt_history_complete"], true);
    assert_eq!(data(&detail)["event_id"], event["id"]);
    assert_eq!(data(&detail)["organization_id"], event["organization_id"]);
    assert_eq!(data(&detail)["environment"], event["environment"]);
    assert_eq!(data(&detail)["workspace"], event["workspace"]);
    assert_eq!(data(&detail)["endpoint_id"], hook);
    assert_eq!(data(&detail)["attempts"], 1);
    assert_eq!(data(&detail)["next_attempt_at"], Value::Null);
    let (status, attempts) = stack
        .api(who, Method::GET, &format!("{path}/attempts?limit=1"), None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{attempts}");
    assert_eq!(attempts["meta"]["pagination"]["total"], 1);
    assert_eq!(attempts["meta"]["pagination"]["next_cursor"], Value::Null);
    let attempt = &data(&attempts)[0];
    assert_eq!(attempt["number"], 1);
    assert_eq!(attempt["status_code"], data(&detail)["last_status_code"]);
    assert_eq!(attempt["error_class"], data(&detail)["last_error"]);
    assert_eq!(attempt["attempted_at"], data(&detail)["last_attempt_at"]);
    assert!(
        attempt["duration_ms"].as_u64().is_some(),
        "duration is recorded: {attempt}"
    );
    for response in [&detail, &attempts] {
        let serialized = response.to_string();
        for secret in secrets {
            assert!(!serialized.contains(*secret), "history never reveals a signing secret");
        }
        assert!(
            !serialized.contains(request.signature.as_deref().unwrap()),
            "history never stores signing headers"
        );
    }
}

/// A resend has its own history, while the event and source stay unchanged.
async fn resend_history(
    stack: &Stack,
    who: &str,
    org: &str,
    hook: &str,
    source: &Value,
    original: &WebhookRequest,
    secrets: &[&str],
) -> Value {
    let path = format!("/v1/organizations/{org}/webhooks/{hook}/deliveries");
    let source_path = format!("{path}/{}", id(source));
    let mut before = Vec::new();
    for suffix in ["", "/attempts"] {
        let (status, body) = stack
            .api(who, Method::GET, &format!("{source_path}{suffix}"), None, None)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        before.push(body);
    }
    let key = format!("resend-{}", id(source));
    let send = || {
        stack
            .http
            .post(format!("{}{source_path}/resend", stack.base))
            .bearer_auth(who)
            .header("Idempotency-Key", key.as_str())
            .send()
    };
    let response = send().await.unwrap();
    let status = response.status();
    let first = response.text().await.unwrap();
    assert_eq!(status, StatusCode::CREATED, "{first}");
    let created: Value = serde_json::from_str(&first).unwrap();
    let delivery = data(&created);
    assert_ne!(delivery["id"], source["id"]);
    for field in [
        "organization_id",
        "endpoint_id",
        "event_id",
        "environment",
        "event_type",
        "event_created_at",
        "workspace",
    ] {
        assert_eq!(delivery[field], source[field], "resend preserves {field}");
    }
    assert_eq!(delivery["state"], "pending");
    assert_eq!(delivery["attempts"], 0);
    for field in ["last_status_code", "last_error", "last_attempt_at"] {
        assert_eq!(delivery[field], Value::Null, "a resend has no previous {field}");
    }
    assert!(delivery["created_at"].is_string());
    assert_eq!(delivery["next_attempt_at"], delivery["created_at"]);

    let resent_path = format!("{path}/{}", id(delivery));
    let completed = eventually("the queued resend to succeed", || async {
        let (status, body) = stack.api(who, Method::GET, &resent_path, None, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_ne!(data(&body)["state"], "failed", "{body}");
        (data(&body)["state"] == "succeeded").then_some(data(&body).clone())
    })
    .await;
    assert_eq!(completed["last_status_code"], 204);
    let requests: Vec<_> = stack
        .taskboard
        .taskboard
        .webhook_requests()
        .into_iter()
        .filter(|request| request.event_id == original.event_id)
        .collect();
    assert_eq!(requests.len(), 2, "the original and one queued resend");
    let resent = &requests[1];
    assert_eq!(
        resent.body, original.body,
        "send the stored bytes without reserializing"
    );
    assert_eq!(resent.event_id, original.event_id);
    assert_eq!(resent.event_type, original.event_type);
    check_history(stack, who, org, hook, &completed, resent, secrets).await;

    // Replay after the worker finishes still returns the original pending
    // response, with no new delivery, attempt or audit event.
    let replay = send().await.unwrap();
    assert_eq!(replay.status(), StatusCode::CREATED);
    assert_eq!(replay.text().await.unwrap(), first);
    let query = source["workspace"]["id"]
        .as_str()
        .map(|workspace| format!("workspace_id={workspace}"))
        .unwrap_or_else(|| "event_type=webhook.test".to_owned());
    let (status, page) = stack
        .api(who, Method::GET, &format!("{path}?{query}"), None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let rows = data(&page).as_array().unwrap();
    assert!(
        rows.iter()
            .all(|row| row["workspace"]["id"] == source["workspace"]["id"])
    );
    let same_event: Vec<_> = rows
        .iter()
        .filter(|row| row["event_id"] == source["event_id"])
        .collect();
    assert_eq!(same_event.len(), 2, "one source and one resend after replay");
    assert!(same_event.iter().any(|row| row["id"] == source["id"]));
    assert!(same_event.iter().any(|row| row["id"] == delivery["id"]));
    for (suffix, expected) in ["", "/attempts"].into_iter().zip(before) {
        let (status, after) = stack
            .api(who, Method::GET, &format!("{source_path}{suffix}"), None, None)
            .await;
        assert_eq!(status, StatusCode::OK, "{after}");
        assert_eq!(after, expected, "the original delivery and its attempts are immutable");
    }
    let (status, me) = stack.api(who, Method::GET, "/v1/me", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, audit) = stack
        .api(
            who,
            Method::GET,
            &format!("/v1/organizations/{org}/audit-events?limit=100"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{audit}");
    let records: Vec<_> = data(&audit)
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["action"] == "webhook.delivery_resent" && event["target"]["id"] == delivery["id"])
        .collect();
    assert_eq!(records.len(), 1, "idempotency replay never audits a second resend");
    let record = records[0];
    assert_eq!(record["organization_id"], org);
    assert_eq!(record["workspace_id"], source["workspace"]["id"]);
    assert_eq!(record["workspace"]["id"], source["workspace"]["id"]);
    assert_eq!(record["environment"], source["environment"]);
    assert_eq!(
        record["actor"],
        json!({ "type": "user", "id": data(&me)["user"]["id"], "name": data(&me)["user"]["name"] })
    );
    assert_eq!(
        record["target"],
        json!({ "type": "webhook_delivery", "id": delivery["id"], "name": source["event_type"] })
    );
    completed
}

/// A completed run in a fresh workspace; fixture jobs can't enter its history.
async fn history_run(stack: &Stack, who: &str, org: &str, slug: &str, environment: &str) -> (String, String) {
    let (status, workspace) = stack
        .api(
            who,
            Method::POST,
            &format!("/v1/organizations/{org}/workspaces"),
            None,
            Some(json!({ "name": slug, "slug": slug, "environment": environment, "external_id": slug })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{workspace}");
    let ws = id(data(&workspace));
    let (status, agent) = stack
        .api(
            who,
            Method::POST,
            &format!("/v1/workspaces/{ws}/agents"),
            Some(org),
            Some(json!({ "name": "Webhook trial", "description": "", "model": "model-balanced", "instructions": "Answer briefly." })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{agent}");
    let caller = if environment == "production" {
        let (status, key) = stack
            .api(
                who,
                Method::POST,
                &format!("/v1/organizations/{org}/api-keys"),
                None,
                Some(json!({ "name": "Webhook trial", "environment": "production" })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{key}");
        data(&key)["secret"].as_str().unwrap().to_owned()
    } else {
        who.to_owned()
    };
    let (status, run) = stack
        .api(
            &caller,
            Method::POST,
            &format!("/v1/workspaces/{ws}/agents/{}/runs", id(data(&agent))),
            Some(org),
            Some(json!({ "input": "A private message that never belongs in a webhook." })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{run}");
    let run = id(data(&run));
    eventually("the history run to finish", || async {
        let (status, body) = stack
            .api(
                who,
                Method::GET,
                &format!("/v1/workspaces/{ws}/runs/{run}"),
                Some(org),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_ne!(data(&body)["status"], "failed", "{body}");
        (data(&body)["status"] == "completed").then_some(())
    })
    .await;
    (ws, run)
}

#[tokio::test]
async fn webhook_history_matches_deliveries_and_keeps_their_scope() {
    let stack = Stack::start().await;
    let board = &stack.taskboard.taskboard;
    let mut organizations = Vec::new();
    for (who, slug) in [(OWNER, "northwind"), ("contract:outsider", "larkspur")] {
        let (status, me) = stack.api(who, Method::GET, "/v1/me", None, None).await;
        assert_eq!(status, StatusCode::OK);
        organizations.push(
            data(&me)["organizations"]
                .as_array()
                .unwrap()
                .iter()
                .find(|org| org["slug"] == slug)
                .map(id)
                .unwrap(),
        );
    }
    let org = &organizations[0];
    let other_org = &organizations[1];
    let (sandbox, sandbox_secret) = history_hook(&stack, OWNER, org, "sandbox").await;
    let (production, production_secret) = history_hook(&stack, OWNER, org, "production").await;
    let (other, other_secret) = history_hook(&stack, "contract:outsider", other_org, "sandbox").await;
    let sandbox_path = format!("/v1/organizations/{org}/webhooks/{sandbox}/deliveries");

    // Both outcomes are actual HTTPS requests, and the synchronous test
    // delivery needs no retry timer to exercise a terminal failure.
    for status_code in [204, 503] {
        board.set_webhook_status(Some(status_code));
        let (status, sent) = stack
            .api(
                OWNER,
                Method::POST,
                &format!("/v1/organizations/{org}/webhooks/{sandbox}/test"),
                None,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{sent}");
        assert_eq!(data(&sent)["status_code"], status_code);
    }
    board.set_webhook_status(None);
    let mut path = format!("{sandbox_path}?event_type=webhook.test&limit=1");
    let mut test_deliveries = Vec::new();
    loop {
        let (status, page) = stack.api(OWNER, Method::GET, &path, None, None).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["meta"]["pagination"]["total"], 2);
        test_deliveries.extend(data(&page).as_array().unwrap().iter().cloned());
        let Some(cursor) = page["meta"]["pagination"]["next_cursor"].as_str() else {
            break;
        };
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("event_type", "webhook.test")
            .append_pair("limit", "1")
            .append_pair("cursor", cursor)
            .finish();
        path = format!("{sandbox_path}?{query}");
        assert!(test_deliveries.len() < 3, "pagination makes progress");
    }
    assert_eq!(test_deliveries.len(), 2);
    assert_ne!(test_deliveries[0]["id"], test_deliveries[1]["id"]);
    for summary in &test_deliveries {
        let failed = summary["last_status_code"] == 503;
        assert_eq!(summary["state"], if failed { "failed" } else { "succeeded" });
        assert_eq!(
            summary["last_error"],
            if failed { json!("status") } else { Value::Null }
        );
        assert_eq!(summary["workspace"], Value::Null);
        let request = board
            .webhook_requests()
            .into_iter()
            .find(|request| request.event_id.as_deref() == summary["event_id"].as_str())
            .unwrap();
        check_history(&stack, OWNER, org, &sandbox, summary, &request, &[&sandbox_secret]).await;
        if failed {
            // The recovered receiver accepts a queued test event even
            // though the endpoint subscribes only to run.completed.
            resend_history(&stack, OWNER, org, &sandbox, summary, &request, &[&sandbox_secret]).await;
        }
    }

    // An organization can inspect both its workspaces, while the filter
    // and each signed event preserve the workspace and its external ID.
    let mut journeys = Vec::new();
    for (who, organization, slug, environment, hook, secret) in [
        (OWNER, org, "webhook-first", "sandbox", &sandbox, &sandbox_secret),
        (OWNER, org, "webhook-second", "sandbox", &sandbox, &sandbox_secret),
        (
            OWNER,
            org,
            "webhook-live",
            "production",
            &production,
            &production_secret,
        ),
        (
            "contract:outsider",
            other_org,
            "webhook-other",
            "sandbox",
            &other,
            &other_secret,
        ),
    ] {
        let (ws, run) = history_run(&stack, who, organization, slug, environment).await;
        let path = format!("/v1/organizations/{organization}/webhooks/{hook}/deliveries?workspace_id={ws}");
        let summary = eventually("the scoped delivery history", || async {
            let (status, page) = stack.api(who, Method::GET, &path, None, None).await;
            assert_eq!(status, StatusCode::OK, "{page}");
            let rows = data(&page).as_array().unwrap();
            assert!(rows.len() <= 1, "only this workspace's event: {page}");
            rows.first().filter(|row| row["state"] == "succeeded").cloned()
        })
        .await;
        assert_eq!(summary["workspace"], json!({ "id": ws, "external_id": slug }));
        assert_eq!(summary["environment"], environment);
        let request = board
            .webhook_requests()
            .into_iter()
            .find(|request| request.event_id.as_deref() == summary["event_id"].as_str())
            .unwrap();
        let event = verify_webhook(&request, &[secret]);
        assert_eq!(event["data"]["id"], run);
        assert!(
            !String::from_utf8_lossy(&request.body).contains("A private message"),
            "the event has no run content"
        );
        check_history(&stack, who, organization, hook, &summary, &request, &[secret]).await;
        let resent = resend_history(&stack, who, organization, hook, &summary, &request, &[secret]).await;
        journeys.push((organization.to_owned(), hook.to_owned(), ws, resent));
    }

    for (organization, hook, ws, summary) in &journeys {
        if hook != &sandbox {
            let (status, filtered) = stack
                .api(
                    OWNER,
                    Method::GET,
                    &format!("{sandbox_path}?workspace_id={ws}"),
                    None,
                    None,
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{filtered}");
            assert!(
                data(&filtered).as_array().unwrap().is_empty(),
                "another environment or org"
            );
            let (status, hidden) = stack
                .api(
                    OWNER,
                    Method::GET,
                    &format!("{sandbox_path}/{}", id(summary)),
                    None,
                    None,
                )
                .await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "a delivery under the wrong endpoint: {hidden}"
            );
            let (status, refused) = stack
                .api(
                    OWNER,
                    Method::POST,
                    &format!("{sandbox_path}/{}/resend", id(summary)),
                    None,
                    None,
                )
                .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "a wrong-endpoint resend: {refused}");
        }
        if organization == other_org {
            let (status, hidden) = stack
                .api(
                    OWNER,
                    Method::GET,
                    &format!("/v1/organizations/{organization}/webhooks/{hook}/deliveries"),
                    None,
                    None,
                )
                .await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "another organization's history: {hidden}"
            );
            let (status, refused) = stack
                .api(
                    OWNER,
                    Method::POST,
                    &format!(
                        "/v1/organizations/{organization}/webhooks/{hook}/deliveries/{}/resend",
                        id(summary)
                    ),
                    None,
                    None,
                )
                .await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "another organization's resend: {refused}"
            );
        }
    }
}

async fn public_hook(stack: &Stack, org: &str, hook: &str, who: &str, environment: &str) -> Value {
    let (status, response) = stack
        .api(
            who,
            Method::GET,
            &format!("/v1/organizations/{org}/webhooks?environment={environment}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !response.to_string().contains("whsec_"),
        "reads never reveal signing secrets"
    );
    data(&response)
        .as_array()
        .unwrap()
        .iter()
        .find(|endpoint| endpoint["id"] == hook)
        .unwrap()
        .clone()
}

async fn roll_secret(stack: &Stack, org: &str, hook: &str, who: &str, key: &str) -> (StatusCode, Value) {
    let response = stack
        .http
        .post(format!(
            "{}/v1/organizations/{org}/webhooks/{hook}/roll-secret",
            stack.base
        ))
        .bearer_auth(who)
        .header("Idempotency-Key", key)
        .send()
        .await
        .unwrap();
    let status = response.status();
    (status, response.json().await.unwrap())
}

#[tokio::test]
async fn webhook_secret_rotation_keeps_overlap_history_and_scope() {
    let stack = Stack::start().await;
    let board = &stack.taskboard.taskboard;
    let mut organizations = Vec::new();
    for (who, slug) in [(OWNER, "northwind"), ("contract:outsider", "larkspur")] {
        let (status, me) = stack.api(who, Method::GET, "/v1/me", None, None).await;
        assert_eq!(status, StatusCode::OK);
        organizations.push(
            data(&me)["organizations"]
                .as_array()
                .unwrap()
                .iter()
                .find(|organization| organization["slug"] == slug)
                .map(id)
                .unwrap(),
        );
    }
    let org = organizations[0].as_str();
    let other_org = organizations[1].as_str();
    let (sandbox, old_secret) = history_hook(&stack, OWNER, org, "sandbox").await;
    let (production, production_secret) = history_hook(&stack, OWNER, org, "production").await;
    let (other, other_secret) = history_hook(&stack, "contract:outsider", other_org, "sandbox").await;
    let endpoints = [
        (OWNER, org, "sandbox", sandbox.as_str(), old_secret.as_str()),
        (
            OWNER,
            org,
            "production",
            production.as_str(),
            production_secret.as_str(),
        ),
        (
            "contract:outsider",
            other_org,
            "sandbox",
            other.as_str(),
            other_secret.as_str(),
        ),
    ];
    let mut keys = Vec::new();
    for environment in ["sandbox", "production"] {
        let (status, created) = stack
            .api(
                OWNER,
                Method::POST,
                &format!("/v1/organizations/{org}/api-keys"),
                None,
                Some(json!({ "name": "Rotation journey", "environment": environment })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED);
        keys.push(data(&created)["secret"].as_str().unwrap().to_owned());
    }

    // Every source has reached a terminal state before rotation: subsequent
    // differences cannot come from an earlier worker finishing its delivery.
    let mut sources = Vec::new();
    let mut history_snapshots = Vec::new();
    for (who, organization, slug, environment, hook, secret) in [
        (
            OWNER,
            org,
            "rotation-first",
            "sandbox",
            sandbox.as_str(),
            old_secret.as_str(),
        ),
        (
            OWNER,
            org,
            "rotation-second",
            "sandbox",
            sandbox.as_str(),
            old_secret.as_str(),
        ),
        (
            OWNER,
            org,
            "rotation-live",
            "production",
            production.as_str(),
            production_secret.as_str(),
        ),
        (
            "contract:outsider",
            other_org,
            "rotation-other",
            "sandbox",
            other.as_str(),
            other_secret.as_str(),
        ),
    ] {
        let (ws, run) = history_run(&stack, who, organization, slug, environment).await;
        let path = format!("/v1/organizations/{organization}/webhooks/{hook}/deliveries");
        let filtered_path = format!("{path}?workspace_id={ws}");
        let summary = eventually("the delivery before rotation", || async {
            let (status, page) = stack.api(who, Method::GET, &filtered_path, None, None).await;
            assert_eq!(status, StatusCode::OK, "{page}");
            let rows = data(&page).as_array().unwrap();
            assert!(rows.len() <= 1, "only this workspace's event");
            rows.first().filter(|row| row["state"] == "succeeded").cloned()
        })
        .await;
        let request = board
            .webhook_requests()
            .into_iter()
            .find(|request| request.event_id.as_deref() == summary["event_id"].as_str())
            .unwrap();
        let event = verify_webhook(&request, &[secret]);
        assert_eq!(event["data"]["id"], run);
        assert_eq!(summary["workspace"], json!({ "id": ws, "external_id": slug }));
        assert_eq!(summary["environment"], environment);
        check_history(&stack, who, organization, hook, &summary, &request, &[secret]).await;
        for path in [
            filtered_path,
            format!("{path}/{}", id(&summary)),
            format!("{path}/{}/attempts", id(&summary)),
        ] {
            let (status, response) = stack.api(who, Method::GET, &path, None, None).await;
            assert_eq!(status, StatusCode::OK, "{response}");
            history_snapshots.push((who, hook, path, response));
        }
        sources.push((who, organization, hook, summary, request));
    }
    let mut endpoint_snapshots = Vec::new();
    for (who, organization, environment, hook, _) in endpoints {
        let endpoint = public_hook(&stack, organization, hook, who, environment).await;
        assert_eq!(endpoint.get("previous_signing_secret_expires_at"), Some(&Value::Null));
        endpoint_snapshots.push(endpoint);
    }
    let requests_before = board.webhook_requests();

    for (index, (who, organization, hook)) in [
        (OWNER, other_org, other.as_str()),
        ("contract:outsider", org, sandbox.as_str()),
        (OWNER, org, other.as_str()),
        (keys[0].as_str(), org, production.as_str()),
        (keys[1].as_str(), org, sandbox.as_str()),
        (keys[0].as_str(), other_org, other.as_str()),
    ]
    .into_iter()
    .enumerate()
    {
        let (status, refused) = roll_secret(&stack, organization, hook, who, &format!("refused-roll-{index}")).await;
        assert!(!refused.to_string().contains("whsec_"));
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(code(&refused), "not_found");
        assert!(data(&refused).is_null());
    }

    // Independent idempotency keys compete for the one previous-secret slot.
    // The outcome is fixed even if the server handles the requests serially.
    let started = Timestamp::now().millis();
    let (first, second) = tokio::join!(
        roll_secret(&stack, org, &sandbox, OWNER, "rotation-first"),
        roll_secret(&stack, org, &sandbox, OWNER, "rotation-second"),
    );
    let finished = Timestamp::now().millis();
    let mut winner = None;
    for (key, (status, response)) in [("rotation-first", first), ("rotation-second", second)] {
        if status == StatusCode::OK {
            assert!(winner.is_none(), "only one concurrent rotation succeeds");
            winner = Some((key, response));
        } else {
            assert!(!response.to_string().contains("whsec_"));
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(code(&response), "rotation_in_progress");
            assert!(data(&response).is_null());
        }
    }
    let (winning_key, rolled) = winner.expect("one rotation succeeds");
    assert_eq!(rolled["meta"], Value::Null);
    assert_eq!(rolled["error"], Value::Null);
    assert_eq!(data(&rolled).as_object().unwrap().len(), 2);
    let new_secret = data(&rolled)["signing_secret"].as_str().unwrap().to_owned();
    assert!(new_secret.starts_with("whsec_test_"));
    assert!(new_secret != old_secret, "rotation issues a different signing secret");
    assert!(
        !rolled.to_string().contains(&old_secret),
        "the previous secret is never returned"
    );
    let endpoint = &data(&rolled)["endpoint"];
    assert!(!endpoint.to_string().contains("whsec_"));
    assert_eq!(endpoint["id"], sandbox);
    assert_eq!(endpoint["signing_secret_last4"], &new_secret[new_secret.len() - 4..]);
    let expires_at = endpoint["previous_signing_secret_expires_at"].as_str().unwrap();
    let overlap_ms = 24 * 60 * 60 * 1000;
    let earliest = Timestamp::from_millis(started + overlap_ms).to_string();
    let latest = Timestamp::from_millis(finished + overlap_ms).to_string();
    assert!(
        expires_at >= earliest.as_str() && expires_at <= latest.as_str(),
        "one 24-hour overlap"
    );

    for (key, expected_code) in [
        (winning_key, "idempotency_key_completed"),
        ("rotation-again", "rotation_in_progress"),
    ] {
        let (status, response) = roll_secret(&stack, org, &sandbox, OWNER, key).await;
        assert!(
            !response.to_string().contains("whsec_"),
            "a retry cannot reveal a secret"
        );
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(code(&response), expected_code);
        assert!(data(&response).is_null());
    }
    assert_eq!(board.webhook_requests(), requests_before, "rotation sends no event");
    for (who, _, path, before) in &history_snapshots {
        let (status, after) = stack.api(who, Method::GET, path, None, None).await;
        assert_eq!(status, StatusCode::OK, "{after}");
        assert_eq!(
            &after, before,
            "rotation leaves every event, delivery and attempt unchanged"
        );
    }
    for ((who, organization, environment, hook, _), before) in endpoints.into_iter().zip(&endpoint_snapshots) {
        let after = public_hook(&stack, organization, hook, who, environment).await;
        let mut expected = before.clone();
        if hook == sandbox {
            expected["signing_secret_last4"] = endpoint["signing_secret_last4"].clone();
            expected["previous_signing_secret_expires_at"] = endpoint["previous_signing_secret_expires_at"].clone();
        }
        assert_eq!(
            after, expected,
            "only the selected endpoint's public signing metadata changes"
        );
    }

    // Old events retain their exact bytes when delivered through the job queue
    // again. Both sibling workspaces use this endpoint's bounded pair of keys.
    let active_secrets = [old_secret.as_str(), new_secret.as_str()];
    for (who, organization, hook, source, original) in &sources {
        if *hook == sandbox {
            resend_history(&stack, who, organization, hook, source, original, &active_secrets).await;
        }
    }
    // The synchronous test path uses the same pair. Neighboring scopes still
    // produce exactly one signature, made with their own original secret.
    for (who, organization, environment, hook, secret) in endpoints {
        let secrets = if hook == sandbox {
            active_secrets.to_vec()
        } else {
            vec![secret]
        };
        let count = board.webhook_requests().len();
        let (status, sent) = stack
            .api(
                who,
                Method::POST,
                &format!("/v1/organizations/{organization}/webhooks/{hook}/test"),
                None,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{sent}");
        assert_eq!(data(&sent)["status_code"], 204);
        let requests = board.webhook_requests();
        assert_eq!(requests.len(), count + 1);
        let request = requests.last().unwrap();
        let event = verify_webhook(request, &secrets);
        assert_eq!(event["organization_id"], organization);
        assert_eq!(event["environment"], environment);
        assert_eq!(event["workspace"], Value::Null);
        assert_eq!(event["data"]["id"], hook);
        let (status, page) = stack
            .api(
                who,
                Method::GET,
                &format!("/v1/organizations/{organization}/webhooks/{hook}/deliveries?event_type=webhook.test"),
                None,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(data(&page).as_array().unwrap().len(), 1);
        check_history(&stack, who, organization, hook, &data(&page)[0], request, &secrets).await;
    }
    for (who, hook, path, before) in &history_snapshots {
        if *hook != sandbox {
            let (status, after) = stack.api(who, Method::GET, path, None, None).await;
            assert_eq!(status, StatusCode::OK, "{after}");
            assert_eq!(&after, before, "neighboring run histories remain untouched");
        }
    }
    let visible = public_hook(&stack, org, &sandbox, OWNER, "sandbox").await;
    assert_eq!(
        visible["previous_signing_secret_expires_at"], expires_at,
        "the overlap was never extended"
    );
    let (status, audit) = stack
        .api(
            OWNER,
            Method::GET,
            &format!("/v1/organizations/{org}/audit-events?limit=100"),
            None,
            None,
        )
        .await;
    assert!(
        !audit.to_string().contains("whsec_"),
        "audit records contain no signing material"
    );
    assert_eq!(status, StatusCode::OK);
    let rolls: Vec<_> = data(&audit)
        .as_array()
        .unwrap()
        .iter()
        .filter(|record| record["action"] == "webhook.secret_rolled" && record["target"]["id"] == sandbox)
        .collect();
    assert_eq!(
        rolls.len(),
        1,
        "conflicting requests and idempotent replay add no rotation audit"
    );
    assert_eq!(rolls[0]["organization_id"], org);
    assert_eq!(rolls[0]["environment"], "sandbox");
    assert_eq!(rolls[0]["workspace_id"], Value::Null);
    assert_eq!(rolls[0]["workspace"], Value::Null);
}

#[tokio::test]
async fn connect_lifecycle_delivers_signed_immutable_outcomes() {
    let stack = Stack::start().await;
    let t = tenant(&stack).await;
    let board = stack.taskboard.taskboard.clone();
    let kinds = [
        "installation.connected",
        "installation.disconnected",
        "connect_session.completed",
        "connect_session.failed",
    ];
    let (hook, secret) = subscribed_hook(&stack, OWNER, &t.org, "sandbox", &kinds).await;
    let (production, _) = subscribed_hook(&stack, OWNER, &t.org, "production", &kinds).await;
    let (_, me) = stack.api("contract:outsider", Method::GET, "/v1/me", None, None).await;
    let other_org = id(&data(&me)["organizations"][0]);
    let (foreign, _) = subscribed_hook(&stack, "contract:outsider", &other_org, "sandbox", &kinds).await;
    let (status, body) = stack
        .api(
            OWNER,
            Method::PATCH,
            &format!("/v1/workspaces/{}", t.ws),
            Some(&t.org),
            Some(json!({ "external_id": "original-tenant" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let cancelled = t.session(&stack, None).await;
    let (status, cancelled_body) = stack
        .api(
            "",
            Method::POST,
            &format!("/v1/connect/sessions/{cancelled}/cancel"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{cancelled_body}");
    let (status, _) = stack
        .api(
            "",
            Method::POST,
            &format!("/v1/connect/sessions/{cancelled}/cancel"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let denied = t.session(&stack, None).await;
    let page = t.consent_page(&stack, &denied).await;
    let denied_body = t.answer(&stack, &denied, &page, "deny").await;
    assert_eq!(denied_body["failure_code"], "consent_denied");
    let reference = t.session(&stack, None).await;
    let page = t.consent_page(&stack, &reference).await;
    let callback = t.decide_consent(&stack, &page, "approve").await;
    let completed = t.follow(&stack, &reference, &callback).await;
    let installation = completed["installation_id"].as_str().unwrap();
    assert_eq!(completed["status"], "completed");
    let (status, _, _) = stack.browse(Method::GET, &callback, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a callback replay emits nothing");
    let reconnect = t.session(&stack, Some(installation)).await;
    let page = t.consent_page(&stack, &reconnect).await;
    let renewed = t.answer(&stack, &reconnect, &page, "approve").await;
    assert_eq!(renewed["installation_id"], installation);
    for _ in 0..2 {
        let (status, body) = stack
            .api(
                OWNER,
                Method::POST,
                &format!("/v1/workspaces/{}/installations/{installation}/disconnect", t.ws),
                Some(&t.org),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, body) = stack
        .api(
            OWNER,
            Method::PATCH,
            &format!("/v1/workspaces/{}", t.ws),
            Some(&t.org),
            Some(json!({ "external_id": "renamed-tenant" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = stack
        .api(
            OWNER,
            Method::DELETE,
            &format!("/v1/workspaces/{}/installations/{installation}", t.ws),
            Some(&t.org),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let path = format!(
        "/v1/organizations/{}/webhooks/{hook}/deliveries?workspace_id={}&limit=100",
        t.org, t.ws
    );
    let history = eventually("all lifecycle events to reach the signed receiver", || async {
        let (status, body) = stack.api(OWNER, Method::GET, &path, None, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let rows = data(&body).as_array().unwrap();
        (rows.len() == 8 && rows.iter().all(|row| row["state"] == "succeeded")).then(|| rows.clone())
    })
    .await;
    let requests = board.webhook_requests();
    let mut actual = Vec::new();
    for summary in &history {
        let request = requests
            .iter()
            .find(|request| request.event_id.as_deref() == summary["event_id"].as_str())
            .unwrap();
        let event = verify_webhook(request, &[&secret]);
        assert_eq!(event["organization_id"], t.org);
        assert_eq!(event["environment"], "sandbox");
        assert_eq!(event["workspace"]["id"], t.ws);
        check_history(&stack, OWNER, &t.org, &hook, summary, request, &[&secret]).await;
        actual.push(event);
    }
    assert_eq!(
        actual
            .iter()
            .filter(|event| event["type"] == "installation.connected")
            .count(),
        2
    );
    assert_eq!(
        actual
            .iter()
            .filter(|event| event["type"] == "installation.disconnected")
            .count(),
        2
    );
    for session in [&completed, &renewed] {
        let event = actual
            .iter()
            .find(|event| event["data"]["id"] == session["id"])
            .unwrap();
        assert_eq!(
            event["data"],
            json!({ "object": "connect_session", "id": session["id"], "status": "completed", "installation_id": installation, "failure_code": null })
        );
    }
    for (session, status, failure) in [
        (data(&cancelled_body), "cancelled", Value::Null),
        (&denied_body, "failed", json!("consent_denied")),
    ] {
        let event = actual
            .iter()
            .find(|event| event["data"]["id"] == session["id"])
            .unwrap();
        assert_eq!(
            event["data"],
            json!({ "object": "connect_session", "id": session["id"], "status": status, "installation_id": null, "failure_code": failure })
        );
    }
    assert_eq!(
        actual
            .iter()
            .filter(|event| event["workspace"]["external_id"] == "original-tenant")
            .count(),
        7
    );
    assert_eq!(
        actual
            .iter()
            .filter(|event| event["workspace"]["external_id"] == "renamed-tenant")
            .count(),
        1
    );
    let exposed = serde_json::to_string(&actual).unwrap();
    for private in [&reference, &reconnect, &cancelled, &denied, &secret] {
        assert!(!exposed.contains(private));
    }
    assert!(!exposed.contains("Robin"));
    for (who, org, endpoint) in [
        (OWNER, t.org.as_str(), production.as_str()),
        ("contract:outsider", other_org.as_str(), foreign.as_str()),
    ] {
        let (_, body) = stack
            .api(
                who,
                Method::GET,
                &format!("/v1/organizations/{org}/webhooks/{endpoint}/deliveries"),
                None,
                None,
            )
            .await;
        assert!(
            data(&body).as_array().unwrap().is_empty(),
            "other scopes receive none of these events: {body}"
        );
    }
    let original = history
        .iter()
        .find(|row| row["event_type"] == "installation.connected")
        .unwrap();
    let request = requests
        .iter()
        .find(|request| request.event_id.as_deref() == original["event_id"].as_str())
        .unwrap();
    resend_history(&stack, OWNER, &t.org, &hook, original, request, &[&secret]).await;
}
