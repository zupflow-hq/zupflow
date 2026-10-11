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
//! expired or revoked token surfaces as the installation needing a reconnect,
//! which a reconnect fixes.
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
use zupflow_taskboard::{Options, Running};

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
            Some(json!({ "kind": "mcp", "name": "Taskboard", "slug": "taskboard", "description": "Tasks, for testing.", "auth_type": "mcp_oauth" })),
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
    assert_eq!(code(&serde_json::from_str(&body).unwrap()), "invalid_oauth_state");
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
    assert_eq!(code(&serde_json::from_str(&body).unwrap()), "invalid_oauth_state");
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
    // Levels are suggested just after the listing is recorded.
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

    // --- An expired access token ------------------------------------------
    // Today the backend never refreshes: the installation keeps the refresh
    // token Taskboard issued, but a run that meets an expired access token
    // fails, asking for a reconnect, and the installation still reads as
    // connected. This changes when refreshing lands (the next change): it
    // becomes a run that completes after one `refresh_token` request.
    board.expire_access_tokens();
    let run = t.start_run(&stack, &agent, "call list_tasks {}").await;
    let failed = t.run_reaches(&stack, &run, "failed").await;
    assert_eq!(failed["error"]["code"], "installation_reconnect_required", "{failed}");
    assert_eq!(
        failed["error"]["message"],
        "Robin's board needs to be reconnected before its tools can run."
    );
    assert!(
        board
            .token_requests()
            .iter()
            .all(|request| request.grant_type != "refresh_token"),
        "no refresh is tried"
    );
    assert_eq!(t.installation(&stack, &installation_id).await["status"], "connected");
    assert_eq!(board.tool_calls().len(), 2, "refused before the tool ran");

    // --- A reconnect signs in again, as the same account ------------------
    let reference = t.session(&stack, Some(&installation_id)).await;
    let page = t.consent_page(&stack, &reference).await;
    let reconnected = t.answer(&stack, &reference, &page, "approve").await;
    assert_eq!(reconnected["status"], "completed", "{reconnected}");
    let installation = t.installation(&stack, &installation_id).await;
    assert_eq!(installation["connection"]["credential_version"], 2);
    let run = t.start_run(&stack, &agent, "call list_tasks {}").await;
    let done = t.run_reaches(&stack, &run, "completed").await;
    assert!(
        done["output"]
            .as_str()
            .unwrap()
            .contains("TB-1 [open] Send the quarterly report"),
        "{done}"
    );
    let renewed = board.access_tokens_issued(&client_id).last().unwrap().clone();
    assert_ne!(renewed, token);
    assert_eq!(
        board.tool_calls().last().unwrap().access_token,
        renewed,
        "with the new token"
    );

    // --- A revoked token: the same, reconnect required --------------------
    board.revoke_all_tokens();
    let run = t.start_run(&stack, &agent, "call list_tasks {}").await;
    let failed = t.run_reaches(&stack, &run, "failed").await;
    assert_eq!(failed["error"]["code"], "installation_reconnect_required", "{failed}");
    let events = t.events(&stack, &run).await;
    assert_eq!(&types(&events)[events.len() - 2..], ["tool_call.failed", "run.failed"]);
    assert_eq!(
        events[events.len() - 2]["data"]["error"]["code"],
        "installation_reconnect_required"
    );

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
